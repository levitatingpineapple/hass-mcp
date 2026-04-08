mod api;
use api::ws::*;
use futures_util::{Sink, SinkExt, StreamExt};
use parking_lot::RwLock;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    service::RequestContext,
    tool, tool_router,
};
use schemars::{JsonSchema, Schema};
use serde::Deserialize;
use serde_json::json;
use std::{collections::HashMap, sync::OnceLock};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, Message},
};

/// MIME type for MCP App HTML resources
const MCP_APP_MIME_TYPE: &str = "text/html;profile=mcp-app";

/// URI for the Hass UI resource
const HASS_URI: &str = "ui://hass/view";

#[derive(Clone)]
pub struct Host {
    host: String,
}

impl Host {
    pub fn new(host: String) -> Self {
        Self { host }
    }

    pub fn api(&self) -> String {
        format!("https://{}/api", self.host)
    }

    pub fn websocket(&self) -> String {
        format!("wss://{}/api/websocket", self.host)
    }
}

static LIGHTS: OnceLock<RwLock<HashMap<Light, bool>>> = OnceLock::new();

pub(crate) fn lights() -> &'static RwLock<HashMap<Light, bool>> {
    LIGHTS.get().expect("lights not initialized")
}

/// A Home Assistant light entity, stored without the `light.` prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Light {
    pub name: String,
}

impl Light {
    fn entity_id(&self) -> String {
        format!("light.{}", self.name)
    }

    fn from_entity_id(id: &str) -> Result<Light, String> {
        Ok(Light {
            name: id
                .strip_prefix("light.")
                .ok_or_else(|| format!("entity id {id:?} is missing the 'light.' prefix"))?
                .to_string(),
        })
    }
}

pub struct Hass {
    uri: Host,
    token: String,
    tool_router: ToolRouter<Hass>,
}

#[tool_router]
impl Hass {
    pub async fn new() -> Self {
        let host = std::env::var("HASS_HOST").expect("HASS_HOST must be set");
        let token = std::env::var("HASS_TOKEN").expect("HASS_TOKEN must be set");
        let uri = Host::new(host);
        let (ws, _) = connect_async(&uri.websocket())
            .await
            .expect("WebSocket connect failed");
        let (mut ws_tx, mut ws_rx) = ws.split();

        macro_rules! recv {
            () => {
                serde_json::from_str::<HassMsg>(
                    ws_rx
                        .next()
                        .await
                        .expect("WS closed")
                        .expect("WS error")
                        .to_text()
                        .expect("not text"),
                )
                .expect("WS parse error")
            };
        }

        // Auth handshake
        match recv!() {
            HassMsg::AuthRequired => {}
            _ => panic!("expected auth_required"),
        }
        send(
            &mut ws_tx,
            &HassCmd::Auth {
                access_token: &token,
            },
        )
        .await
        .expect("send auth failed");
        match recv!() {
            HassMsg::AuthOk => tracing::info!("WebSocket authenticated"),
            HassMsg::AuthInvalid { message } => panic!("auth invalid: {message}"),
            _ => panic!("expected auth response"),
        }

        // Fetch initial light states
        send(&mut ws_tx, &HassCmd::GetStates { id: 1 })
            .await
            .expect("send get_states failed");
        match recv!() {
            HassMsg::Result { id: 1, result } => {
                let map: HashMap<Light, bool> = result
                    .into_iter()
                    .flatten()
                    .filter_map(|e| {
                        Light::from_entity_id(&e.entity_id)
                            .ok()
                            .map(|l| (l, e.state == "on"))
                    })
                    .collect();
                LIGHTS
                    .set(RwLock::new(map))
                    .expect("lights already initialized");
            }
            _ => panic!("expected get_states result"),
        }

        // Subscribe to state changes and spawn event listener
        send(
            &mut ws_tx,
            &HassCmd::SubscribeEvents {
                id: 2,
                event_type: "state_changed",
            },
        )
        .await
        .expect("send subscribe failed");
        tokio::spawn(async move {
            let _tx = ws_tx;
            while let Some(Ok(raw)) = ws_rx.next().await {
                let Ok(text) = raw.to_text() else { continue };
                if let Ok(HassMsg::Event { id: 2, event }) = serde_json::from_str(text) {
                    let s = event.data.new_state;
                    if let Ok(light) = Light::from_entity_id(&s.entity_id) {
                        lights().write().insert(light, s.state == "on");
                    }
                }
            }
            tracing::error!("WebSocket connection closed");
        });

        Self {
            uri,
            token,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Turns light on or off. Passing only name returns current state",
        meta = Meta(rmcp::object!({
            "ui": {
                "resourceUri": HASS_URI
            }
        }))
    )]
    async fn light(
        &self,
        Parameters(LightParams { name, is_on }): Parameters<LightParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(is_on) = is_on {
            let state = if is_on { "on" } else { "off" };
            if lights()
                .read()
                .get(&Light { name: name.clone() })
                .expect("present")
                == &is_on
            {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Light {name} was already {state}.",
                ))]));
            };
            let url = format!(
                "{}/services/light/{}",
                self.uri.api(),
                if is_on { "turn_on" } else { "turn_off" }
            );
            let client = reqwest::Client::new();
            let light: Light = Light { name: name.clone() };
            client
                .post(&url)
                .bearer_auth(&self.token)
                .json(&json!({ "entity_id": light.entity_id() }))
                .send()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .error_for_status()
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            // Update local state instantly after a successful call
            // as it can take time for ws state to "come back"
            lights().write().insert(light, is_on);

            Ok(CallToolResult::success(vec![Content::text(format!(
                "Light {name} turned {state}.",
            ))]))
        } else {
            let light = Light { name };
            let map = lights().read();
            let state = map
                .get(&light)
                .map(|&on| if on { "On" } else { "Off" })
                .unwrap_or("Unknown");
            Ok(CallToolResult::success(vec![Content::text(state)]))
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum WsError {
    #[error(transparent)]
    Tungstenite(#[from] tungstenite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

async fn send(
    tx: &mut (impl Sink<Message, Error = tungstenite::Error> + Unpin),
    msg: &HassCmd<'_>,
) -> Result<(), WsError> {
    let json_string = serde_json::to_string(msg)?;
    Ok(tx.send(Message::Text(json_string.into())).await?)
}

#[rmcp::tool_handler]
impl ServerHandler for Hass {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_resources()
                .enable_tools()
                .build(),
        )
        .with_server_info(Implementation::from_build_env())
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(
            "This server provides a Home Assistant light toggle via MCP.".to_string(),
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let resource = RawResource::new(HASS_URI, "Light Toggle")
            .with_mime_type(MCP_APP_MIME_TYPE)
            .with_description("Toggle for the light")
            .with_meta(Meta(rmcp::object!({
                "ui": {
                    "prefersBorder": false
                }
            })))
            .no_annotation();
        Ok(ListResourcesResult {
            resources: vec![resource],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = &request.uri;
        match uri.as_str() {
            HASS_URI => {
                let html = std::fs::read_to_string("/home/user/repo/local/mcp/src/view.html")
                    .map_err(|e| {
                        McpError::resource_not_found(
                            "resource_not_found",
                            Some(json!({ "error": e.to_string() })),
                        )
                    })?;
                let contents = ResourceContents::text(html, uri.clone())
                    .with_mime_type(MCP_APP_MIME_TYPE)
                    .with_meta(Meta(rmcp::object!({
                        "ui": {
                            "prefersBorder": false
                        }
                    })));
                Ok(ReadResourceResult::new(vec![contents]))
            }
            _ => Err(McpError::resource_not_found(
                "resource_not_found",
                Some(json!({ "uri": uri })),
            )),
        }
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult {
            next_cursor: None,
            resource_templates: Vec::new(),
            meta: None,
        })
    }
}

fn restrict_to_light_ids(schema: &mut Schema) {
    let map = crate::hass::lights().read();
    let names: Vec<_> = map.keys().map(|l| json!(l.name)).collect();
    schema.insert("enum".to_owned(), json!(names));
}

#[derive(Deserialize, JsonSchema)]
pub struct LightParams {
    #[schemars(transform = restrict_to_light_ids)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_on: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_params_schema_enum() {
        LIGHTS.get_or_init(|| {
            let mut map = HashMap::new();
            map.insert(Light { name: "foo".into() }, false);
            RwLock::new(map)
        });
        let schema = schemars::schema_for!(LightParams);
        let json = serde_json::to_string_pretty(&schema).unwrap();
        assert_eq!(
            json,
            r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "LightParams",
  "type": "object",
  "properties": {
    "is_on": {
      "type": [
        "boolean",
        "null"
      ]
    },
    "name": {
      "type": "string",
      "enum": [
        "foo"
      ]
    }
  },
  "required": [
    "name"
  ]
}"#
        );
    }
}

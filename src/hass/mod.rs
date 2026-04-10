mod api;
use api::ws::*;
use futures_util::{Sink, SinkExt, StreamExt};
use light::Light;
use parking_lot::RwLock;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    service::RequestContext,
    tool, tool_router,
};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::{json, to_value};
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

mod light {

    /// A Home Assistant light entity.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct Light {
        name: String,
    }

    impl Light {
        pub fn entity_id(&self) -> String {
            format!("light.{}", self.name)
        }

        pub fn name(&self) -> String {
            self.name.clone()
        }

        /// Creates light form WS
        pub fn from_entity_id(id: &str) -> Result<Light, String> {
            Ok(Light {
                name: id
                    .strip_prefix("light.")
                    .ok_or_else(|| format!("entity id {id:?} is missing the 'light.' prefix"))?
                    .to_string(),
            })
        }

        /// Creates a light form MCP parameters
        pub fn from_params(params: &super::LightParams) -> Light {
            Light {
                name: params.name.clone(),
            }
        }
    }
}

#[derive(Clone)]
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
                tracing::info!("Setting LIGHTS");
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

    #[tool(description = "Just a demo")]
    async fn picture(&self) -> Result<CallToolResult, McpError> {
        let base64_image = "/9j/4AAQSkZJRgABAQEBLAEsAAD/4QBWRXhpZgAATU0AKgAAAAgABAEaAAUAAAABAAAAPgEbAAUAAAABAAAARgEoAAMAAAABAAIAAAITAAMAAAABAAEAAAAAAAAAAAEsAAAAAQAAASwAAAAB/+0JeFBob3Rvc2hvcCAzLjAAOEJJTQQEAAAAAAlbHAFaAAMbJUccAQAAAgAEHAIAAAIABBwCBQBMU21hbGwgZG9nIGJlaW5nIGZlZCBpY2UgY3JlYW0gaW4gY29uZS4gS0kgZ2VuZXJpZXJ0LCBnZW5lcmllcnQsIEFJIGdlbmVyYXRlZBwCGQAIZGVzZXJ0ZWQcAhkABm5vIG9uZRwCGQAJTm8gUGVvcGxlHAIZAAZub2JvZHkcAhkAC3VuaW5oYWJpdGVkHAIZAAVlbXB0eRwCGQAGbGl0dGxlHAIZAAVzbWFsbBwCGQAEdGlueRwCGQADaWNlHAIZAAVzbmFjaxwCGQAGc25hY2tzHAIZAAVsdW5jaBwCGQAHbHVuY2hlcxwCGQAHZGVzc2VydBwCGQAIZGVzc2VydHMcAhkACWljZSBjcmVhbRwCGQAJaWNlLWNyZWFtHAIZAAppY2UgY3JlYW1zHAIZAAhpY2VjcmVhbRwCGQAHZGVzc2VydBwCGQAIZGVzc2VydHMcAhkAB2ZlZWRpbmccAhkACGZlZWRpbmdzHAIZAARmZWVkHAIZAAZmZWVkZXIcAhkACHRvZ2V0aGVyHAIZAA93aXRoIGVhY2ggb3RoZXIcAhkABmZyaWVuZBwCGQAHZnJpZW5kcxwCGQAEaGFuZBwCGQAFaGFuZHMcAhkACWhhbmQgb3ZlchwCGQAIcGVkaWdyZWUcAhkADHRob3JvdWdoYnJlZBwCGQAIcHVyZWJyZWQcAhkABmFuaW1hbBwCGQAHYW5pbWFscxwCGQAHZmVlZGluZxwCGQAHZmVlZGluZxwCGQAJaW5nZXN0aW9uHAIZAANlYXQcAhkABmVhdGluZxwCGQAEZWF0cxwCGQAEZm9vZBwCGQAEZm9vZBwCGQAVZm9vZHN0dWZmLCBmb29kc3R1ZmZzHAIZAAdncm9jZXJ5HAIZAAlncm9jZXJpZXMcAhkABk1hbW1hbBwCGQAITWFtbWFsaWEcAhkAB01hbW1hbHMcAhkACU1hbW1hbGlhbhwCGQAKTWFtbWFsaWFucxwCGQAFdHJlYXQcAhkACHRyZWF0aW5nHAIZAAZ0cmVhdHMcAhkABnBlcnNvbhwCGQAHcGVyc29ucxwCGQALSHVtYW4gYmVpbmccAhkABlBlb3BsZRwCGQAFaHVtYW4cAhkABmh1bWFucxwCGQAXaW5kaXZpZHVhbCwgaW5kaXZpZHVhbHMcAhkABXNoYXJlHAIZAAdzaGFyaW5nHAIZAAZzaGFyZXMcAhkAB3NoYXJpbmccAhkABGZlZWQcAhkAB2ZlZWRpbmccAhkABWZlZWRzHAIZAAphcHBldGl6aW5nHAIZAAphcHBldGlzaW5nHAIZAAdzYXZvdXJ5HAIZAAZzYXZvcnkcAhkACWRlbGljaW91cxwCGQAMYXBwZXRpemluZ2x5HAIZAAxBcHBldGlzaW5nbHkcAhkACmZyaWVuZHNoaXAcAhkAC2ZyaWVuZHNoaXBzHAIZAAZodW5ncnkcAhkAA2ZlZBwCGQAPaWNlLWNyZWFtIHdhZmVyHAIZABBpY2UtY3JlYW0gd2FmZXJzHAIZAA9pY2UgY3JlYW0gd2FmZXIcAhkADmljZSBjcmVhbSBjb25lHAIZAAdob2xkaW5nHAIZAAhob2xkaW5ncxwCGQAIZG9nIGZvb2QcAhkACGRvZyBmb29kHAIZAAV0YXN0eRwCGQAMRG9tZXN0aWMgZG9nHAIZAA1Eb21lc3RpYyBkb2dzHAIZAANkb2ccAhkABGRvZ3McAhkAEENhbmlzIGZhbWlsaWFyaXMcAhkAFkNhbmlzIGx1cHVzIGZhbWlsaWFyaXMcAhkADHdoaWxlIGVhdGluZxwCGQAHZmVlZGluZxwCGQAGZWF0aW5nHAIZAA9kb21lc3RpYyBhbmltYWwcAhkAEGRvbWVzdGljIGFuaW1hbHMcAhkAA1BldBwCGQAEcGV0cxwCGQAMQWkgZ2VuZXJhdGVkHAIZABBBaSBnZW5lcmF0ZWQgYXJ0HAIZAA1haSBnZW5lcmF0aXZlHAIZAAxBaSBnZW5lcmF0ZXMcAhkADWdlbmVyYXRpdmUgQUkcAhkADEFpIGdlbmVyaWVydBwCGQAMS2kgZ2VuZXJhdGVkHAIZAAxHZW5lcmF0ZWQgQUkcAhkADEtJIGdlbmVyYXRlcxwCGQAkZ2VuZXJhdGVkIHdpdGggQUksIGdlbmVyYXRlZCB3aXRoIEtJHAIZACBBcnRpZmljYWwgSW50ZWxsaWdlbmNlIGdlbmVyYXRlZBwCGQAMQUkgZ2VuZXJhdGVkHAIZAAhkZXNlcnRlZBwCGQAGbGl0dGxlHAIZAANpY2UcAhkABXNuYWNrHAIZAAVsdW5jaBwCGQAHZGVzc2VydBwCGQAJaWNlIGNyZWFtHAIZAAdmZWVkaW5nHAIZAAh0b2dldGhlchwCGQAGZnJpZW5kHAIZAARoYW5kHAIZAAhwZWRpZ3JlZRwCGQAGYW5pbWFsHAIZAANlYXQcAhkABGZvb2QcAhkABm1hbW1hbBwCGQAIbWFtbWFsaWEcAhkABXRyZWF0HAIZAAZwZXJzb24cAhkABXNoYXJlHAIZAARmZWVkHAIZAAphcHBldGl6aW5nHAIZAApmcmllbmRzaGlwHAIZAAZodW5ncnkcAhkAA2ZlZBwCGQAPaWNlLWNyZWFtIHdhZmVyHAIZAAdob2xkaW5nHAIZAAhkb2cgZm9vZBwCGQAFdGFzdHkcAhkADGRvbWVzdGljIGRvZxwCGQAQY2FuaXMgZmFtaWxpYXJpcxwCGQAMd2hpbGUgZWF0aW5nHAIZAANwZXQcAhkADGFpIGdlbmVyYXRlZBwCGQAMYWkgZ2VuZXJhdGVkHAIoAAtNUj1ObyxQUj1ObxwCNwAIMjAyNDAzMjEcAlAAEGltYWdlQlJPS0VSL0Zpcm4cAmUAB0dlcm1hbnkcAmcAEmlieG1vYjEwMzc1Nzc1LmpwZxwCbgAiRmlybi9pbWFnZUJST0tFUiAtIHN0b2NrLmFkb2JlLmNvbRwCcwAKMTIyOTEyODMxORwCdAAPaW1hZ2VCUk9LRVIuY29tHAJ4AExTbWFsbCBkb2cgYmVpbmcgZmVkIGljZSBjcmVhbSBpbiBjb25lLiBLSSBnZW5lcmllcnQsIGdlbmVyaWVydCwgQUkgZ2VuZXJhdGVkHAKHAAJFTgD/4R/UaHR0cDovL25zLmFkb2JlLmNvbS94YXAvMS4wLwA8P3hwYWNrZXQgYmVnaW49J++7vycgaWQ9J1c1TTBNcENlaGlIenJlU3pOVGN6a2M5ZCc/Pgo8eDp4bXBtZXRhIHhtbG5zOng9J2Fkb2JlOm5zOm1ldGEvJyB4OnhtcHRrPSdJbWFnZTo6RXhpZlRvb2wgMTEuODgnPgo8cmRmOlJERiB4bWxuczpyZGY9J2h0dHA6Ly93d3cudzMub3JnLzE5OTkvMDIvMjItcmRmLXN5bnRheC1ucyMnPgoKIDxyZGY6RGVzY3JpcHRpb24gcmRmOmFib3V0PScnCiAgeG1sbnM6ZGM9J2h0dHA6Ly9wdXJsLm9yZy9kYy9lbGVtZW50cy8xLjEvJz4KICA8ZGM6ZGVzY3JpcHRpb24+CiAgIDxyZGY6QWx0PgogICAgPHJkZjpsaSB4bWw6bGFuZz0neC1kZWZhdWx0Jz5TbWFsbCBkb2cgYmVpbmcgZmVkIGljZSBjcmVhbSBpbiBjb25lLiBLSSBnZW5lcmllcnQsIGdlbmVyaWVydCwgQUkgZ2VuZXJhdGVkPC9yZGY6bGk+CiAgIDwvcmRmOkFsdD4KICA8L2RjOmRlc2NyaXB0aW9uPgogIDxkYzpzdWJqZWN0PgogICA8cmRmOkJhZz4KICAgIDxyZGY6bGk+ZGVzZXJ0ZWQ8L3JkZjpsaT4KICAgIDxyZGY6bGk+bm8gb25lPC9yZGY6bGk+CiAgICA8cmRmOmxpPk5vIFBlb3BsZTwvcmRmOmxpPgogICAgPHJkZjpsaT5ub2JvZHk8L3JkZjpsaT4KICAgIDxyZGY6bGk+dW5pbmhhYml0ZWQ8L3JkZjpsaT4KICAgIDxyZGY6bGk+ZW1wdHk8L3JkZjpsaT4KICAgIDxyZGY6bGk+bGl0dGxlPC9yZGY6bGk+CiAgICA8cmRmOmxpPnNtYWxsPC9yZGY6bGk+CiAgICA8cmRmOmxpPnRpbnk8L3JkZjpsaT4KICAgIDxyZGY6bGk+aWNlPC9yZGY6bGk+CiAgICA8cmRmOmxpPnNuYWNrPC9yZGY6bGk+CiAgICA8cmRmOmxpPnNuYWNrczwvcmRmOmxpPgogICAgPHJkZjpsaT5sdW5jaDwvcmRmOmxpPgogICAgPHJkZjpsaT5sdW5jaGVzPC9yZGY6bGk+CiAgICA8cmRmOmxpPmRlc3NlcnQ8L3JkZjpsaT4KICAgIDxyZGY6bGk+ZGVzc2VydHM8L3JkZjpsaT4KICAgIDxyZGY6bGk+aWNlIGNyZWFtPC9yZGY6bGk+CiAgICA8cmRmOmxpPmljZS1jcmVhbTwvcmRmOmxpPgogICAgPHJkZjpsaT5pY2UgY3JlYW1zPC9yZGY6bGk+CiAgICA8cmRmOmxpPmljZWNyZWFtPC9yZGY6bGk+CiAgICA8cmRmOmxpPmRlc3NlcnQ8L3JkZjpsaT4KICAgIDxyZGY6bGk+ZGVzc2VydHM8L3JkZjpsaT4KICAgIDxyZGY6bGk+ZmVlZGluZzwvcmRmOmxpPgogICAgPHJkZjpsaT5mZWVkaW5nczwvcmRmOmxpPgogICAgPHJkZjpsaT5mZWVkPC9yZGY6bGk+CiAgICA8cmRmOmxpPmZlZWRlcjwvcmRmOmxpPgogICAgPHJkZjpsaT50b2dldGhlcjwvcmRmOmxpPgogICAgPHJkZjpsaT53aXRoIGVhY2ggb3RoZXI8L3JkZjpsaT4KICAgIDxyZGY6bGk+ZnJpZW5kPC9yZGY6bGk+CiAgICA8cmRmOmxpPmZyaWVuZHM8L3JkZjpsaT4KICAgIDxyZGY6bGk+aGFuZDwvcmRmOmxpPgogICAgPHJkZjpsaT5oYW5kczwvcmRmOmxpPgogICAgPHJkZjpsaT5oYW5kIG92ZXI8L3JkZjpsaT4KICAgIDxyZGY6bGk+cGVkaWdyZWU8L3JkZjpsaT4KICAgIDxyZGY6bGk+dGhvcm91Z2hicmVkPC9yZGY6bGk+CiAgICA8cmRmOmxpPnB1cmVicmVkPC9yZGY6bGk+CiAgICA8cmRmOmxpPmFuaW1hbDwvcmRmOmxpPgogICAgPHJkZjpsaT5hbmltYWxzPC9yZGY6bGk+CiAgICA8cmRmOmxpPmZlZWRpbmc8L3JkZjpsaT4KICAgIDxyZGY6bGk+ZmVlZGluZzwvcmRmOmxpPgogICAgPHJkZjpsaT5pbmdlc3Rpb248L3JkZjpsaT4KICAgIDxyZGY6bGk+ZWF0PC9yZGY6bGk+CiAgICA8cmRmOmxpPmVhdGluZzwvcmRmOmxpPgogICAgPHJkZjpsaT5lYXRzPC9yZGY6bGk+CiAgICA8cmRmOmxpPmZvb2Q8L3JkZjpsaT4KICAgIDxyZGY6bGk+Zm9vZDwvcmRmOmxpPgogICAgPHJkZjpsaT5mb29kc3R1ZmYsIGZvb2RzdHVmZnM8L3JkZjpsaT4KICAgIDxyZGY6bGk+Z3JvY2VyeTwvcmRmOmxpPgogICAgPHJkZjpsaT5ncm9jZXJpZXM8L3JkZjpsaT4KICAgIDxyZGY6bGk+TWFtbWFsPC9yZGY6bGk+CiAgICA8cmRmOmxpPk1hbW1hbGlhPC9yZGY6bGk+CiAgICA8cmRmOmxpPk1hbW1hbHM8L3JkZjpsaT4KICAgIDxyZGY6bGk+TWFtbWFsaWFuPC9yZGY6bGk+CiAgICA8cmRmOmxpPk1hbW1hbGlhbnM8L3JkZjpsaT4KICAgIDxyZGY6bGk+dHJlYXQ8L3JkZjpsaT4KICAgIDxyZGY6bGk+dHJlYXRpbmc8L3JkZjpsaT4KICAgIDxyZGY6bGk+dHJlYXRzPC9yZGY6bGk+CiAgICA8cmRmOmxpPnBlcnNvbjwvcmRmOmxpPgogICAgPHJkZjpsaT5wZXJzb25zPC9yZGY6bGk+CiAgICA8cmRmOmxpPkh1bWFuIGJlaW5nPC9yZGY6bGk+CiAgICA8cmRmOmxpPlBlb3BsZTwvcmRmOmxpPgogICAgPHJkZjpsaT5odW1hbjwvcmRmOmxpPgogICAgPHJkZjpsaT5odW1hbnM8L3JkZjpsaT4KICAgIDxyZGY6bGk+aW5kaXZpZHVhbCwgaW5kaXZpZHVhbHM8L3JkZjpsaT4KICAgIDxyZGY6bGk+c2hhcmU8L3JkZjpsaT4KICAgIDxyZGY6bGk+c2hhcmluZzwvcmRmOmxpPgogICAgPHJkZjpsaT5zaGFyZXM8L3JkZjpsaT4KICAgIDxyZGY6bGk+c2hhcmluZzwvcmRmOmxpPgogICAgPHJkZjpsaT5mZWVkPC9yZGY6bGk+CiAgICA8cmRmOmxpPmZlZWRpbmc8L3JkZjpsaT4KICAgIDxyZGY6bGk+ZmVlZHM8L3JkZjpsaT4KICAgIDxyZGY6bGk+YXBwZXRpemluZzwvcmRmOmxpPgogICAgPHJkZjpsaT5hcHBldGlzaW5nPC9yZGY6bGk+CiAgICA8cmRmOmxpPnNhdm91cnk8L3JkZjpsaT4KICAgIDxyZGY6bGk+c2F2b3J5PC9yZGY6bGk+CiAgICA8cmRmOmxpPmRlbGljaW91czwvcmRmOmxpPgogICAgPHJkZjpsaT5hcHBldGl6aW5nbHk8L3JkZjpsaT4KICAgIDxyZGY6bGk+QXBwZXRpc2luZ2x5PC9yZGY6bGk+CiAgICA8cmRmOmxpPmZyaWVuZHNoaXA8L3JkZjpsaT4KICAgIDxyZGY6bGk+ZnJpZW5kc2hpcHM8L3JkZjpsaT4KICAgIDxyZGY6bGk+aHVuZ3J5PC9yZGY6bGk+CiAgICA8cmRmOmxpPmZlZDwvcmRmOmxpPgogICAgPHJkZjpsaT5pY2UtY3JlYW0gd2FmZXI8L3JkZjpsaT4KICAgIDxyZGY6bGk+aWNlLWNyZWFtIHdhZmVyczwvcmRmOmxpPgogICAgPHJkZjpsaT5pY2UgY3JlYW0gd2FmZXI8L3JkZjpsaT4KICAgIDxyZGY6bGk+aWNlIGNyZWFtIGNvbmU8L3JkZjpsaT4KICAgIDxyZGY6bGk+aG9sZGluZzwvcmRmOmxpPgogICAgPHJkZjpsaT5ob2xkaW5nczwvcmRmOmxpPgogICAgPHJkZjpsaT5kb2cgZm9vZDwvcmRmOmxpPgogICAgPHJkZjpsaT5kb2cgZm9vZDwvcmRmOmxpPgogICAgPHJkZjpsaT50YXN0eTwvcmRmOmxpPgogICAgPHJkZjpsaT5Eb21lc3RpYyBkb2c8L3JkZjpsaT4KICAgIDxyZGY6bGk+RG9tZXN0aWMgZG9nczwvcmRmOmxpPgogICAgPHJkZjpsaT5kb2c8L3JkZjpsaT4KICAgIDxyZGY6bGk+ZG9nczwvcmRmOmxpPgogICAgPHJkZjpsaT5DYW5pcyBmYW1pbGlhcmlzPC9yZGY6bGk+CiAgICA8cmRmOmxpPkNhbmlzIGx1cHVzIGZhbWlsaWFyaXM8L3JkZjpsaT4KICAgIDxyZGY6bGk+d2hpbGUgZWF0aW5nPC9yZGY6bGk+CiAgICA8cmRmOmxpPmZlZWRpbmc8L3JkZjpsaT4KICAgIDxyZGY6bGk+ZWF0aW5nPC9yZGY6bGk+CiAgICA8cmRmOmxpPmRvbWVzdGljIGFuaW1hbDwvcmRmOmxpPgogICAgPHJkZjpsaT5kb21lc3RpYyBhbmltYWxzPC9yZGY6bGk+CiAgICA8cmRmOmxpPlBldDwvcmRmOmxpPgogICAgPHJkZjpsaT5wZXRzPC9yZGY6bGk+CiAgICA8cmRmOmxpPkFpIGdlbmVyYXRlZDwvcmRmOmxpPgogICAgPHJkZjpsaT5BaSBnZW5lcmF0ZWQgYXJ0PC9yZGY6bGk+CiAgICA8cmRmOmxpPmFpIGdlbmVyYXRpdmU8L3JkZjpsaT4KICAgIDxyZGY6bGk+QWkgZ2VuZXJhdGVzPC9yZGY6bGk+CiAgICA8cmRmOmxpPmdlbmVyYXRpdmUgQUk8L3JkZjpsaT4KICAgIDxyZGY6bGk+QWkgZ2VuZXJpZXJ0PC9yZGY6bGk+CiAgICA8cmRmOmxpPktpIGdlbmVyYXRlZDwvcmRmOmxpPgogICAgPHJkZjpsaT5HZW5lcmF0ZWQgQUk8L3JkZjpsaT4KICAgIDxyZGY6bGk+S0kgZ2VuZXJhdGVzPC9yZGY6bGk+CiAgICA8cmRmOmxpPmdlbmVyYXRlZCB3aXRoIEFJLCBnZW5lcmF0ZWQgd2l0aCBLSTwvcmRmOmxpPgogICAgPHJkZjpsaT5BcnRpZmljYWwgSW50ZWxsaWdlbmNlIGdlbmVyYXRlZDwvcmRmOmxpPgogICAgPHJkZjpsaT5BSSBnZW5lcmF0ZWQ8L3JkZjpsaT4KICAgPC9yZGY6QmFnPgogIDwvZGM6c3ViamVjdD4KIDwvcmRmOkRlc2NyaXB0aW9uPgoKIDxyZGY6RGVzY3JpcHRpb24gcmRmOmFib3V0PScnCiAgeG1sbnM6cGhvdG9zaG9wPSdodHRwOi8vbnMuYWRvYmUuY29tL3Bob3Rvc2hvcC8xLjAvJz4KICA8cGhvdG9zaG9wOkluc3RydWN0aW9ucz5NUj1ObyxQUj1ObzwvcGhvdG9zaG9wOkluc3RydWN0aW9ucz4KICA8cGhvdG9zaG9wOlRyYW5zbWlzc2lvblJlZmVyZW5jZT5pYnhtb2IxMDM3NTc3NS5qcGc8L3Bob3Rvc2hvcDpUcmFuc21pc3Npb25SZWZlcmVuY2U+CiA8L3JkZjpEZXNjcmlwdGlvbj4KCiA8cmRmOkRlc2NyaXB0aW9uIHJkZjphYm91dD0nJwogIHhtbG5zOnBsdXM9J2h0dHA6Ly9ucy51c2VwbHVzLm9yZy9sZGYveG1wLzEuMC8nPgogIDxwbHVzOkltYWdlQ3JlYXRvcj4KICAgPHJkZjpTZXE+CiAgICA8cmRmOmxpIHJkZjpwYXJzZVR5cGU9J1Jlc291cmNlJz4KICAgICA8cGx1czpJbWFnZUNyZWF0b3JOYW1lPkZpcm48L3BsdXM6SW1hZ2VDcmVhdG9yTmFtZT4KICAgIDwvcmRmOmxpPgogICA8L3JkZjpTZXE+CiAgPC9wbHVzOkltYWdlQ3JlYXRvcj4KICA8cGx1czpJbWFnZVN1cHBsaWVyPgogICA8cmRmOlNlcT4KICAgIDxyZGY6bGkgcmRmOnBhcnNlVHlwZT0nUmVzb3VyY2UnPgogICAgIDxwbHVzOkltYWdlU3VwcGxpZXJOYW1lPmltYWdlQlJPS0VSL0Zpcm48L3BsdXM6SW1hZ2VTdXBwbGllck5hbWU+CiAgICA8L3JkZjpsaT4KICAgPC9yZGY6U2VxPgogIDwvcGx1czpJbWFnZVN1cHBsaWVyPgogPC9yZGY6RGVzY3JpcHRpb24+CgogPHJkZjpEZXNjcmlwdGlvbiByZGY6YWJvdXQ9JycKICB4bWxuczpwdXI9J2h0dHA6Ly9wcmlzbXN0YW5kYXJkLm9yZy9uYW1lc3BhY2VzL3ByaXNtdXNhZ2VyaWdodHMvMi4xLyc+CiAgPHB1cjpjcmVkaXRMaW5lPgogICA8cmRmOkJhZz4KICAgIDxyZGY6bGk+Rmlybi9pbWFnZUJST0tFUiAtIHN0b2NrLmFkb2JlLmNvbTwvcmRmOmxpPgogICA8L3JkZjpCYWc+CiAgPC9wdXI6Y3JlZGl0TGluZT4KIDwvcmRmOkRlc2NyaXB0aW9uPgoKIDxyZGY6RGVzY3JpcHRpb24gcmRmOmFib3V0PScnCiAgeG1sbnM6dGlmZj0naHR0cDovL25zLmFkb2JlLmNvbS90aWZmLzEuMC8nPgogIDx0aWZmOkJpdHNQZXJTYW1wbGU+CiAgIDxyZGY6U2VxPgogICAgPHJkZjpsaT44PC9yZGY6bGk+CiAgIDwvcmRmOlNlcT4KICA8L3RpZmY6Qml0c1BlclNhbXBsZT4KICA8dGlmZjpJbWFnZUxlbmd0aD4yMDAwPC90aWZmOkltYWdlTGVuZ3RoPgogIDx0aWZmOkltYWdlV2lkdGg+MzAwMDwvdGlmZjpJbWFnZVdpZHRoPgogIDx0aWZmOlJlc29sdXRpb25Vbml0PjI8L3RpZmY6UmVzb2x1dGlvblVuaXQ+CiAgPHRpZmY6WFJlc29sdXRpb24+MzAwLzE8L3RpZmY6WFJlc29sdXRpb24+CiAgPHRpZmY6WUNiQ3JTdWJTYW1wbGluZz4KICAgPHJkZjpTZXE+CiAgICA8cmRmOmxpPjI8L3JkZjpsaT4KICAgIDxyZGY6bGk+MjwvcmRmOmxpPgogICA8L3JkZjpTZXE+CiAgPC90aWZmOllDYkNyU3ViU2FtcGxpbmc+CiAgPHRpZmY6WVJlc29sdXRpb24+MzAwLzE8L3RpZmY6WVJlc29sdXRpb24+CiA8L3JkZjpEZXNjcmlwdGlvbj4KPC9yZGY6UkRGPgo8L3g6eG1wbWV0YT4KICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIAogICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgCiAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAKICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIAogICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgCiAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAKICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIAogICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgCiAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAKICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIAogICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgCiAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAKICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIAogICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgCiAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAKICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIAogICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgCiAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAKICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIAogICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgCiAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAKICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIAogICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgCiAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAKPD94cGFja2V0IGVuZD0ndyc/Pv/bAEMABgQEBQQEBgUFBQYGBgcJDgkJCAgJEg0NCg4VEhYWFRIUFBcaIRwXGB8ZFBQdJx0fIiMlJSUWHCksKCQrISQlJP/bAEMBBgYGCQgJEQkJESQYFBgkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk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";
        let image_content = Content::image(base64_image.to_string(), "image/jpeg".to_string());
        Ok(CallToolResult::success(vec![image_content]))
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
        Parameters(params): Parameters<LightParams>,
    ) -> Result<CallToolResult, McpError> {
        let was_on = lights()
            .read()
            .get(&Light::from_params(&params))
            .cloned()
            .expect("Shema only allows valid light names");
        if let Some(is_on) = params.is_on {
            let url = format!(
                "{}/services/light/{}",
                self.uri.api(),
                if is_on { "turn_on" } else { "turn_off" }
            );
            let client = reqwest::Client::new();
            let light: Light = Light::from_params(&params);
            client
                .post(&url)
                .bearer_auth(&self.token)
                .json(&json!({ "entity_id": light.entity_id() }))
                .send()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .error_for_status()
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        };
        let new_state = params.is_on.unwrap_or(was_on);
        let mut result = CallToolResult::success(vec![Content::text(format!(
            "The {} light {} {}.",
            params.name,
            match params.is_on {
                Some(is_on) => {
                    if was_on == is_on {
                        "was already"
                    } else {
                        "has been tunrned"
                    }
                }
                None => "is",
            },
            if new_state { "on" } else { "off" }
        ))]);
        result.structured_content = Some(
            to_value(LightStructuredContent {
                name: params.name,
                is_on: new_state,
            })
            .expect("Valid JSON"),
        );
        Ok(result)
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
                let html = include_str!("../view.html").to_string();
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
    let mut names: Vec<String> = map.keys().map(|l| l.name().clone()).collect();
    names.sort();
    schema.insert("enum".to_owned(), json!(names));
}

fn optional_bool_schema(_generator: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({"type": "boolean"})
}

#[derive(Deserialize, JsonSchema)]
pub struct LightParams {
    #[schemars(transform = restrict_to_light_ids)]
    pub name: String,
    #[schemars(schema_with = "optional_bool_schema")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_on: Option<bool>,
}

#[derive(Serialize)]
pub struct LightStructuredContent {
    name: String,
    is_on: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_params_schema_enum() {
        LIGHTS.get_or_init(|| {
            RwLock::new(HashMap::from([
                (Light::from_entity_id("light.bar").unwrap(), false),
                (Light::from_entity_id("light.foo").unwrap(), false),
            ]))
        });
        let schema = schemars::schema_for!(LightParams);
        let json: serde_json::Value = serde_json::to_value(&schema).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "LightParams",
                "type": "object",
                "properties": {
                    "is_on": { "type": "boolean" },
                    "name": {
                        "type": "string",
                        "enum": ["bar", "foo"]
                    }
                },
                "required": ["name"]
            })
        );
    }
}

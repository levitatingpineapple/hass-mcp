/// Home assistant websocket API
pub mod ws {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum HassCmd<'a> {
        Auth { access_token: &'a str },
        GetStates { id: u32 },
        SubscribeEvents { id: u32, event_type: &'static str },
    }

    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum HassMsg {
        AuthRequired,
        AuthOk,
        AuthInvalid {
            message: String,
        },
        Result {
            id: u32,
            result: Option<Vec<EntityState>>,
        },
        Event {
            id: u32,
            event: StateChangedEvent,
        },
    }

    #[derive(Deserialize)]
    pub struct EntityState {
        pub entity_id: String,
        pub state: String,
    }

    #[derive(Deserialize)]
    pub struct StateChangedEvent {
        pub data: StateChangedData,
    }

    #[derive(Deserialize)]
    pub struct StateChangedData {
        pub new_state: EntityState,
    }
}

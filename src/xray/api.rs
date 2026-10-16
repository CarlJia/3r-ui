//! xray's gRPC API: the vendored protos behind it, and the live config edits
//! the panel performs through it.
//!
//! xray has no config-reload signal — SIGUSR1 is accepted and ignored, SIGHUP
//! terminates the process (both measured against 26.3.27) — so a change to the
//! user list of a *running* xray has to go through
//! `HandlerService.AlterInbound`. Structural changes (a new inbound, a port,
//! REALITY keys, routing) cannot be applied at all this way: the supervisor
//! restarts the child for those.
//!
//! The API is served by an inbound tagged [`API_TAG`] and is unauthenticated,
//! which is why [`API_ADDR`] is bound to loopback.
//!
//! [`API_TAG`]: crate::xray::config::API_TAG
//! [`API_ADDR`]: crate::xray::config::API_ADDR

use std::time::Duration;

use prost::Message;
use tonic::transport::Endpoint;

use crate::error::{AppError, AppResult};

/// Vendored protobuf trees, mounted where their package paths say they live:
/// the generated code refers to sibling packages through `super::…` chains, so
/// the nesting has to mirror `xray.app.…` / `xray.common.…` / `xray.proxy.…`.
pub mod proto {
    pub mod xray {
        pub mod app {
            pub mod proxyman {
                pub mod command {
                    tonic::include_proto!("xray.app.proxyman.command");
                }
            }
            pub mod stats {
                pub mod command {
                    tonic::include_proto!("xray.app.stats.command");
                }
            }
        }
        pub mod common {
            pub mod protocol {
                tonic::include_proto!("xray.common.protocol");
            }
            pub mod serial {
                tonic::include_proto!("xray.common.serial");
            }
        }
        pub mod proxy {
            pub mod vless {
                tonic::include_proto!("xray.proxy.vless");
            }
        }
    }
}

use proto::xray::app::proxyman::command::{
    handler_service_client::HandlerServiceClient, AddUserOperation, AlterInboundRequest,
    RemoveUserOperation,
};
use proto::xray::common::protocol::User;
use proto::xray::common::serial::TypedMessage;
use proto::xray::proxy::vless::Account;

/// Type names xray resolves packed messages by. They are the proto message
/// names verbatim: xray looks them up in its own registry, so a typo here only
/// surfaces at runtime, as an unknown type.
const ADD_USER_OPERATION: &str = "xray.app.proxyman.command.AddUserOperation";
const REMOVE_USER_OPERATION: &str = "xray.app.proxyman.command.RemoveUserOperation";
const VLESS_ACCOUNT: &str = "xray.proxy.vless.Account";

/// Bound on one API call. A wedged xray must not hold the supervisor's command
/// loop: a restart applies the config too, just with a connection blip.
const API_TIMEOUT: Duration = Duration::from_secs(5);

/// Add a VLESS user to the inbound tagged `tag` of the running xray.
pub async fn add_user(server: &str, tag: &str, uuid: &str, email: &str) -> AppResult<()> {
    alter_inbound(server, tag, add_user_operation(uuid, email)).await
}

/// Remove the user with this `email` from a running inbound.
pub async fn remove_user(server: &str, tag: &str, email: &str) -> AppResult<()> {
    alter_inbound(server, tag, remove_user_operation(email)).await
}

/// The add-user operation, packed the way xray expects it.
///
/// The account mirrors what a *static* config produces for
/// `{"id": …, "email": …, "level": 0}`: an id, and empty `flow`/`encryption`.
/// Under `AddUser` xray builds the same in-memory account from these bytes as
/// it does from that JSON, so the two paths cannot drift.
fn add_user_operation(uuid: &str, email: &str) -> TypedMessage {
    let account = Account {
        id: uuid.to_string(),
        flow: String::new(),
        encryption: String::new(),
    };
    let user = User {
        level: 0,
        email: email.to_string(),
        account: Some(TypedMessage {
            r#type: VLESS_ACCOUNT.to_string(),
            value: account.encode_to_vec(),
        }),
    };
    TypedMessage {
        r#type: ADD_USER_OPERATION.to_string(),
        value: AddUserOperation { user: Some(user) }.encode_to_vec(),
    }
}

fn remove_user_operation(email: &str) -> TypedMessage {
    TypedMessage {
        r#type: REMOVE_USER_OPERATION.to_string(),
        value: RemoveUserOperation {
            email: email.to_string(),
        }
        .encode_to_vec(),
    }
}

async fn alter_inbound(server: &str, tag: &str, operation: TypedMessage) -> AppResult<()> {
    let endpoint = Endpoint::from_shared(format!("http://{server}"))
        .map_err(|e| AppError::Xray(format!("bad api endpoint {server}: {e}")))?
        .timeout(API_TIMEOUT);
    let channel = endpoint
        .connect()
        .await
        .map_err(|e| AppError::Xray(format!("xray api connect {server} failed: {e}")))?;

    HandlerServiceClient::new(channel)
        .alter_inbound(AlterInboundRequest {
            tag: tag.to_string(),
            operation: Some(operation),
        })
        .await
        .map_err(|status| {
            AppError::Xray(format!(
                "xray api AlterInbound({tag}) failed: {}",
                status.message()
            ))
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "11111111-1111-1111-1111-111111111111";

    /// The wire shape is the whole contract with xray: the type names select
    /// the handler, and the field numbers carry the account. Decoding what we
    /// encode is the only way to catch a rename or a renumbered field without
    /// a live xray.
    #[test]
    fn add_user_operation_carries_a_vless_account() {
        let operation = add_user_operation(UUID, "user@example.com");
        assert_eq!(operation.r#type, ADD_USER_OPERATION);

        let parsed = AddUserOperation::decode(operation.value.as_slice()).unwrap();
        let user = parsed.user.expect("the operation must carry a user");
        assert_eq!(user.email, "user@example.com");
        assert_eq!(user.level, 0);

        let account = user.account.expect("the user must carry an account");
        assert_eq!(account.r#type, VLESS_ACCOUNT);
        let vless = Account::decode(account.value.as_slice()).unwrap();
        assert_eq!(vless.id, UUID);
        assert_eq!(vless.flow, "", "VLESS+XHTTP has no flow");
        assert_eq!(vless.encryption, "", "mirrors the static config's account");
    }

    #[test]
    fn remove_user_operation_carries_the_email() {
        let operation = remove_user_operation("user@example.com");
        assert_eq!(operation.r#type, REMOVE_USER_OPERATION);
        let parsed = RemoveUserOperation::decode(operation.value.as_slice()).unwrap();
        assert_eq!(parsed.email, "user@example.com");
    }
}

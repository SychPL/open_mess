//! The administrative API outside Windows (spec 2026-11-03-paperclip-plugin, 5.4): what the
//! configuration window does on Windows, as `POST /admin/{command}` with a JSON body of arguments,
//! for the open_mess plugin in Paperclip. Its own token guards it (api.rs). Every command is a
//! window command (ui/commands.rs); nothing here decides anything of its own.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ui::UiState;
use crate::ui::commands::{self, AccountDto, AnswerDto};

#[derive(Clone)]
struct Admin {
    ui: Arc<UiState>,
    /// The sign-in started last, for `sign_in_result`.
    sign_in: Arc<Mutex<SignInResult>>,
}

/// Where the sign-in started last stands.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum SignInResult {
    #[default]
    Idle,
    Running,
    Done {
        account: AccountDto,
    },
    Failed {
        error: String,
    },
}

/// The routes, on the window's state.
pub fn router(ui: Arc<UiState>) -> Router {
    Router::new().route("/admin/{command}", post(call)).with_state(Admin { ui, sign_in: Arc::default() })
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

#[derive(Deserialize)]
struct Account {
    account: String,
}

#[derive(Deserialize)]
struct ApproveModule {
    name: String,
    file_sha256: String,
    manifest_sha256: String,
}

#[derive(Deserialize)]
struct ModuleEnabled {
    name: String,
    enabled: bool,
}

#[derive(Deserialize)]
struct SetPermissions {
    account: String,
    conversation: Option<String>,
    read: bool,
    write: String,
}

#[derive(Deserialize)]
struct Override {
    account: String,
    conversation: String,
}

#[derive(Deserialize)]
struct Decide {
    action: String,
    approve: bool,
}

#[derive(Deserialize)]
struct SignInStart {
    module: String,
    account: Option<String>,
}

#[derive(Deserialize)]
struct SignInAnswer {
    id: u64,
    answer: AnswerDto,
}

async fn call(State(admin): State<Admin>, Path(command): Path<String>, body: Bytes) -> Response {
    let args = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(args) => args,
            Err(err) => return error(StatusCode::BAD_REQUEST, format!("arguments are not valid JSON: {err}")),
        }
    };
    let ui = &*admin.ui;
    match command.as_str() {
        "status" => answer(commands::status(ui)),
        "accounts" => answer(commands::accounts(ui)),
        "remove_account" => match parse::<Id>(args) {
            Ok(Id { id }) => answer(commands::remove_account(ui, &id).await),
            Err(bad) => error(StatusCode::BAD_REQUEST, bad),
        },
        "modules" => answer(commands::modules(ui)),
        "approve_module" => match parse::<ApproveModule>(args) {
            Ok(module) => answer(commands::approve_module(ui, &module.name, &module.file_sha256, &module.manifest_sha256).await),
            Err(bad) => error(StatusCode::BAD_REQUEST, bad),
        },
        "set_module_enabled" => match parse::<ModuleEnabled>(args) {
            Ok(ModuleEnabled { name, enabled }) => answer(commands::set_module_enabled(ui, &name, enabled).await),
            Err(bad) => error(StatusCode::BAD_REQUEST, bad),
        },
        "rescan_modules" => answer(commands::rescan_modules(ui).await),
        "permissions" => match parse::<Account>(args) {
            Ok(Account { account }) => answer(commands::permissions(ui, &account)),
            Err(bad) => error(StatusCode::BAD_REQUEST, bad),
        },
        "set_permissions" => match parse::<SetPermissions>(args) {
            Ok(set) => answer(commands::set_permissions(ui, &set.account, set.conversation, set.read, &set.write)),
            Err(bad) => error(StatusCode::BAD_REQUEST, bad),
        },
        "clear_permission_override" => match parse::<Override>(args) {
            Ok(Override { account, conversation }) => answer(commands::clear_permission_override(ui, &account, &conversation)),
            Err(bad) => error(StatusCode::BAD_REQUEST, bad),
        },
        "waiting" => Json(commands::waiting(ui)).into_response(),
        "decide" => match parse::<Decide>(args) {
            Ok(Decide { action, approve }) => answer(commands::decide(ui, &action, approve).await),
            Err(bad) => error(StatusCode::BAD_REQUEST, bad),
        },
        "sign_in_start" => match parse::<SignInStart>(args) {
            Ok(start) => sign_in_start(&admin, start),
            Err(bad) => error(StatusCode::BAD_REQUEST, bad),
        },
        "sign_in_request" => Json(commands::sign_in_request(ui)).into_response(),
        "sign_in_answer" => match parse::<SignInAnswer>(args) {
            Ok(SignInAnswer { id, answer: reply }) => answer(commands::sign_in_answer(ui, id, reply)),
            Err(bad) => error(StatusCode::BAD_REQUEST, bad),
        },
        "sign_in_result" => Json(admin.sign_in.lock().unwrap_or_else(PoisonError::into_inner).clone()).into_response(),
        _ => error(StatusCode::NOT_FOUND, format!("unknown command: {command}")),
    }
}

/// Starts a sign-in and answers at once. The first step runs here, so a second sign-in started
/// meanwhile is refused now; the module signs in in a task, whose result `sign_in_result` reads.
/// The task writes it before the sign-in ends, so the result of an older sign-in never lands on a
/// newer one.
fn sign_in_start(admin: &Admin, start: SignInStart) -> Response {
    let active = match commands::sign_in_begin(&admin.ui, &start.module) {
        Ok(active) => active,
        Err(err) => return error(StatusCode::BAD_REQUEST, err),
    };
    *admin.sign_in.lock().unwrap_or_else(PoisonError::into_inner) = SignInResult::Running;
    let admin = admin.clone();
    tokio::spawn(async move {
        let result = match commands::sign_in_run(&admin.ui, &start.module, start.account.as_deref()).await {
            Ok(account) => SignInResult::Done { account },
            Err(error) => SignInResult::Failed { error },
        };
        *admin.sign_in.lock().unwrap_or_else(PoisonError::into_inner) = result;
        drop(active);
    });
    Json(json!({"started": true})).into_response()
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|err| format!("invalid arguments: {err}"))
}

fn answer<T: Serialize>(result: Result<T, String>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(err) => error(StatusCode::BAD_REQUEST, err),
    }
}

fn error(status: StatusCode, message: String) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

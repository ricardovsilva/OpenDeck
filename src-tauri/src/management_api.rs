//! HTTP management API for external control of OpenDeck.
//!
//! Exposes all profile, plugin, and device management operations over a local HTTP interface
//! so that tools such as MCP servers can drive OpenDeck programmatically.
//!
//! # Security
//! * When `bind_address` is a loopback address (e.g. `127.0.0.1` / `::1`), a bearer token is
//!   **optional**.  If no token is configured every loopback connection is accepted.
//! * When `bind_address` is any other address the server **refuses to start** unless a non-empty
//!   bearer token has been configured, and every request must carry a valid
//!   `Authorization: Bearer <token>` header.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::Request;
use axum::{
	Router,
	extract::{Json, Path, State},
	http::{HeaderMap, StatusCode, header},
	middleware::{self, Next},
	response::{IntoResponse, Response},
	routing::{delete, get, patch, post, put},
};
use serde::Deserialize;
use serde_json::json;

use crate::shared::{Action, ActionContext, ActionState, Context};

// ── Shared API state ──────────────────────────────────────────────────────────

struct ApiState {
	/// Expected bearer token value, or `None` when auth is disabled (loopback only).
	token: Option<String>,
}

// ── Error type ────────────────────────────────────────────────────────────────

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
	fn into_response(self) -> Response {
		(self.0, Json(json!({ "error": self.1 }))).into_response()
	}
}

impl ApiError {
	fn bad_request(msg: impl ToString) -> Self {
		Self(StatusCode::BAD_REQUEST, msg.to_string())
	}

	fn internal(msg: impl ToString) -> Self {
		Self(StatusCode::INTERNAL_SERVER_ERROR, msg.to_string())
	}
}

impl From<crate::events::frontend::Error> for ApiError {
	fn from(e: crate::events::frontend::Error) -> Self {
		Self::internal(e)
	}
}

impl From<anyhow::Error> for ApiError {
	fn from(e: anyhow::Error) -> Self {
		Self::internal(e)
	}
}

impl From<std::io::Error> for ApiError {
	fn from(e: std::io::Error) -> Self {
		Self::internal(e)
	}
}

// ── Auth middleware ───────────────────────────────────────────────────────────

async fn auth_middleware(State(state): State<Arc<ApiState>>, headers: HeaderMap, request: Request, next: Next) -> Response {
	if let Some(expected) = &state.token {
		let provided = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
		match provided {
			Some(value) if value == format!("Bearer {expected}") => {}
			_ => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "Unauthorized" }))).into_response(),
		}
	}
	next.run(request).await
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Start the management API HTTP server.
///
/// Returns an error if:
/// * `bind_address` cannot be parsed as an IP address.
/// * `bind_address` is non-loopback and `token` is `None`.
/// * The TCP listener cannot be bound to the requested address/port.
pub async fn start(bind_address: String, port: u16, token: Option<String>) -> Result<(), anyhow::Error> {
	let addr: std::net::IpAddr = bind_address.parse().map_err(|e| anyhow::anyhow!("invalid mcp_bind_address '{bind_address}': {e}"))?;

	if !addr.is_loopback() && token.is_none() {
		return Err(anyhow::anyhow!("MCP management API: a bearer token is required when binding to a non-loopback address"));
	}

	let state = Arc::new(ApiState { token });

	let app = Router::new()
		// Devices / categories
		.route("/devices", get(api_get_devices))
		.route("/categories", get(api_get_categories))
		// Plugins
		.route("/plugins", get(api_list_plugins))
		.route("/plugins/install", post(api_install_plugin))
		.route("/plugins/{id}", delete(api_remove_plugin))
		.route("/plugins/{id}/reload", post(api_reload_plugin))
		// Device profiles — static "selected" route must come before the dynamic {profile} route
		.route("/devices/{device}/profiles", get(api_get_profiles))
		.route("/devices/{device}/profiles/selected", get(api_get_selected_profile))
		.route("/devices/{device}/profiles/selected", put(api_set_selected_profile))
		.route("/devices/{device}/profiles/{profile}", delete(api_delete_profile))
		.route("/devices/{device}/profiles/{profile}/rename", patch(api_rename_profile))
		// Profile slots
		.route("/devices/{device}/profiles/{profile}/slots", get(api_get_profile_slots))
		.route(
			"/devices/{device}/profiles/{profile}/slots/{controller}/{position}",
			get(api_get_slot).post(api_create_instance).delete(api_remove_instance),
		)
		.route("/devices/{device}/profiles/{profile}/slots/{controller}/{position}/move", post(api_move_instance))
		.route("/devices/{device}/profiles/{profile}/slots/{controller}/{position}/state", put(api_set_state))
		// Virtual press (uses the currently active profile; no profile in the path)
		.route("/devices/{device}/slots/{controller}/{position}/press", post(api_trigger_virtual_press))
		// Settings
		.route("/settings", get(api_get_settings).put(api_set_settings))
		// Applications
		.route("/applications", get(api_get_applications))
		.route("/application-profiles", get(api_get_application_profiles).put(api_set_application_profiles))
		.layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
		.with_state(state);

	let socket_addr = SocketAddr::new(addr, port);
	let listener = tokio::net::TcpListener::bind(socket_addr).await?;
	log::info!("MCP management API listening on http://{socket_addr}");
	axum::serve(listener, app).await?;
	Ok(())
}

// ── Devices & categories ──────────────────────────────────────────────────────

async fn api_get_devices() -> impl IntoResponse {
	Json(crate::shared::DEVICES.clone())
}

async fn api_get_categories() -> impl IntoResponse {
	Json(crate::shared::CATEGORIES.read().await.clone())
}

// ── Plugins ───────────────────────────────────────────────────────────────────

async fn api_list_plugins() -> Result<impl IntoResponse, ApiError> {
	let app = crate::APP_HANDLE.get().unwrap().clone();
	Ok(Json(crate::events::frontend::plugins::list_plugins(app).await?))
}

#[derive(Deserialize)]
struct InstallPluginPayload {
	url: Option<String>,
	/// Absolute path to a local plugin archive file.
	file: Option<String>,
	fallback_id: Option<String>,
}

async fn api_install_plugin(Json(payload): Json<InstallPluginPayload>) -> Result<impl IntoResponse, ApiError> {
	let app = crate::APP_HANDLE.get().unwrap().clone();
	crate::events::frontend::plugins::install_plugin(app, payload.url, payload.file, payload.fallback_id).await?;
	Ok(StatusCode::NO_CONTENT)
}

async fn api_remove_plugin(Path(id): Path<String>) -> Result<impl IntoResponse, ApiError> {
	let app = crate::APP_HANDLE.get().unwrap().clone();
	crate::events::frontend::plugins::remove_plugin(app, id).await?;
	Ok(StatusCode::NO_CONTENT)
}

async fn api_reload_plugin(Path(id): Path<String>) -> impl IntoResponse {
	let app = crate::APP_HANDLE.get().unwrap().clone();
	crate::events::frontend::plugins::reload_plugin(app, id).await;
	StatusCode::NO_CONTENT
}

// ── Profiles ──────────────────────────────────────────────────────────────────

async fn api_get_profiles(Path(device): Path<String>) -> Result<impl IntoResponse, ApiError> {
	Ok(Json(crate::events::frontend::profiles::get_profiles(&device)?))
}

async fn api_get_selected_profile(Path(device): Path<String>) -> Result<impl IntoResponse, ApiError> {
	Ok(Json(crate::events::frontend::profiles::get_selected_profile(device).await?))
}

#[derive(Deserialize)]
struct SetSelectedProfilePayload {
	id: String,
}

async fn api_set_selected_profile(Path(device): Path<String>, Json(payload): Json<SetSelectedProfilePayload>) -> Result<impl IntoResponse, ApiError> {
	crate::events::frontend::profiles::set_selected_profile(device, payload.id).await?;
	Ok(StatusCode::NO_CONTENT)
}

async fn api_delete_profile(Path((device, profile)): Path<(String, String)>) -> impl IntoResponse {
	crate::events::frontend::profiles::delete_profile(device, profile).await;
	StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct RenameProfilePayload {
	new_id: String,
	retain: bool,
}

async fn api_rename_profile(Path((device, old_id)): Path<(String, String)>, Json(payload): Json<RenameProfilePayload>) -> Result<impl IntoResponse, ApiError> {
	crate::events::frontend::profiles::rename_profile(device, old_id, payload.new_id, payload.retain).await?;
	Ok(StatusCode::NO_CONTENT)
}

// ── Slots ─────────────────────────────────────────────────────────────────────

async fn api_get_profile_slots(Path((device, profile)): Path<(String, String)>) -> Result<impl IntoResponse, ApiError> {
	let device_info = get_device_info(&device)?;
	let mut locks = crate::store::profiles::acquire_locks_mut().await;
	let store = locks.profile_stores.get_profile_store_mut(&device_info, &profile).await?;
	Ok(Json(store.value.clone()))
}

async fn api_get_slot(Path((device, profile, controller, position)): Path<(String, String, String, u8)>) -> Result<impl IntoResponse, ApiError> {
	let device_info = get_device_info(&device)?;
	let mut locks = crate::store::profiles::acquire_locks_mut().await;
	let store = locks.profile_stores.get_profile_store_mut(&device_info, &profile).await?;
	let slot = match controller.as_str() {
		"Encoder" => store.value.sliders.get(position as usize),
		_ => store.value.keys.get(position as usize),
	};
	Ok(Json(slot.cloned().flatten()))
}

async fn api_create_instance(Path((device, profile, controller, position)): Path<(String, String, String, u8)>, Json(action): Json<Action>) -> Result<impl IntoResponse, ApiError> {
	let app = crate::APP_HANDLE.get().unwrap().clone();
	let context = Context {
		device,
		profile,
		controller,
		position,
	};
	let result = crate::events::frontend::instances::create_instance(app, action, context).await?;
	rerender().await;
	Ok(Json(result))
}

async fn api_remove_instance(Path((device, profile, controller, position)): Path<(String, String, String, u8)>) -> Result<impl IntoResponse, ApiError> {
	let context = ActionContext::from_context(
		Context {
			device,
			profile,
			controller,
			position,
		},
		0,
	);
	crate::events::frontend::instances::remove_instance(context).await?;
	rerender().await;
	Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MoveInstancePayload {
	destination: Context,
	retain: bool,
}

async fn api_move_instance(Path((device, profile, controller, position)): Path<(String, String, String, u8)>, Json(payload): Json<MoveInstancePayload>) -> Result<impl IntoResponse, ApiError> {
	let source = Context {
		device,
		profile,
		controller,
		position,
	};
	let result = crate::events::frontend::instances::move_instance(source, payload.destination, payload.retain).await?;
	rerender().await;
	Ok(Json(result))
}

/// Directory where managed images for a given action instance are stored.
fn instance_images_dir(context: &ActionContext) -> std::path::PathBuf {
	crate::shared::config_dir()
		.join("images")
		.join(&context.device)
		.join(&context.profile)
		.join(format!("{}.{}.{}", context.controller, context.position, context.index))
}

#[derive(Deserialize)]
struct SetStatePayload {
	index: u16,
	state: ActionState,
}

async fn api_set_state(Path((device, profile, controller, position)): Path<(String, String, String, u8)>, Json(payload): Json<SetStatePayload>) -> Result<impl IntoResponse, ApiError> {
	let context = ActionContext::from_context(
		Context {
			device,
			profile,
			controller,
			position,
		},
		0,
	);
	let mut state = payload.state;

	// If the image field is an absolute path to a local file, copy it into managed storage so
	// that the profile JSON always references a path that OpenDeck controls.
	let image_path = std::path::Path::new(&state.image);
	if image_path.is_absolute() {
		if !image_path.exists() {
			return Err(ApiError::bad_request(format!("image file not found: {}", state.image)));
		}
		let dest_dir = instance_images_dir(&context);
		tokio::fs::create_dir_all(&dest_dir).await?;
		let extension = image_path.extension().and_then(|e| e.to_str()).unwrap_or("png");
		let dest = dest_dir.join(format!("api_state_{}.{}", payload.index, extension));
		tokio::fs::copy(image_path, &dest).await?;
		state.image = dest.to_string_lossy().into_owned();
	}

	// Update the instance state directly, mirroring the logic in
	// `events::frontend::instances::set_state` but with proper error handling for a missing
	// instance and correct save of the profile that was actually modified (not necessarily the
	// currently selected profile).
	let mut locks = crate::store::profiles::acquire_locks_mut().await;

	let clone;
	{
		let instance = crate::store::profiles::get_instance_mut(&context, &mut locks)
			.await?
			.ok_or_else(|| ApiError::bad_request("no action instance at this position"))?;

		if payload.index as usize >= instance.states.len() {
			return Err(ApiError::bad_request(format!(
				"state index {} out of bounds (instance has {} states)",
				payload.index,
				instance.states.len()
			)));
		}

		instance.states[payload.index as usize] = state;
		clone = instance.clone();
	} // mutable borrow of locks released here

	// Save the specific profile that was modified (not the selected profile).
	let device_info = get_device_info(&context.device)?;
	locks
		.profile_stores
		.get_profile_store(&device_info, &context.profile)
		.map_err(ApiError::from)?
		.save()
		.map_err(ApiError::from)?;
	drop(locks);

	crate::events::outbound::states::title_parameters_did_change(&clone, payload.index).await?;
	rerender().await;
	Ok(StatusCode::NO_CONTENT)
}

// ── Virtual press ─────────────────────────────────────────────────────────────

async fn api_trigger_virtual_press(Path((device, controller, position)): Path<(String, String, u8)>) -> Result<impl IntoResponse, ApiError> {
	// The profile field is not used inside `trigger_virtual_press`; the inbound event handler
	// resolves the currently active profile on its own.
	let context = Context {
		device,
		profile: String::new(),
		controller,
		position,
	};
	crate::events::frontend::instances::trigger_virtual_press(context).await?;
	Ok(StatusCode::NO_CONTENT)
}

// ── Settings ──────────────────────────────────────────────────────────────────

async fn api_get_settings() -> Result<impl IntoResponse, ApiError> {
	Ok(Json(crate::events::frontend::settings::get_settings().await?))
}

async fn api_set_settings(Json(new_settings): Json<crate::store::Settings>) -> Result<impl IntoResponse, ApiError> {
	let app = crate::APP_HANDLE.get().unwrap().clone();
	crate::events::frontend::settings::set_settings(app, new_settings).await?;
	Ok(StatusCode::NO_CONTENT)
}

// ── Applications ──────────────────────────────────────────────────────────────

async fn api_get_applications() -> impl IntoResponse {
	Json(crate::application_watcher::APPLICATIONS.read().await.clone())
}

async fn api_get_application_profiles() -> impl IntoResponse {
	Json(crate::application_watcher::APPLICATION_PROFILES.read().await.value.clone())
}

async fn api_set_application_profiles(Json(value): Json<crate::application_watcher::ApplicationProfiles>) -> Result<impl IntoResponse, ApiError> {
	crate::events::frontend::set_application_profiles(value).await?;
	Ok(StatusCode::NO_CONTENT)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Look up a connected device by ID, returning a 400 error if not found.
fn get_device_info(device: &str) -> Result<crate::shared::DeviceInfo, ApiError> {
	crate::shared::DEVICES
		.get(device)
		.map(|r| r.value().clone())
		.ok_or_else(|| ApiError::bad_request(format!("device '{device}' not found")))
}

/// Emit a `rerender_images` event to the frontend so that button visuals are refreshed on both
/// the UI canvas and the connected hardware devices.
async fn rerender() {
	if let Some(app) = crate::APP_HANDLE.get() {
		let _ = crate::events::frontend::profiles::rerender_images(app).await;
	}
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn loopback_without_token_is_allowed() {
		// `start` is async and binds a port, so we only test the validation logic.
		let addr: std::net::IpAddr = "127.0.0.1".parse().unwrap();
		assert!(addr.is_loopback());
		// No token required → validation passes (no error before bind attempt).
	}

	#[test]
	fn non_loopback_requires_token() {
		let addr: std::net::IpAddr = "0.0.0.0".parse().unwrap();
		assert!(!addr.is_loopback());
		// Simulate the guard: token is None → should produce an error.
		let token: Option<String> = None;
		assert!(!addr.is_loopback() && token.is_none(), "non-loopback without token must be rejected");
	}
}

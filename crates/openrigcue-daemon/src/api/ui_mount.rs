//! The UI mount: the UI application (`openrigcue_ui::app::MyApp`) runs in this program, on its own threads, and
//! reaches the service through direct function calls.
//! The page, its websocket and the static files are served at `/`.

use std::sync::{Arc, OnceLock};

use openrigcue_model::native_value::LegacyValue;

use axum::body::{to_bytes, Bytes};
use axum::http::header;
use axum::Router;
use openrigcue_ui::backend::{Backend, Failure, HardwareNotification, HardwareNotificationState, Method};
use openrigcue_ui::host::UiHost;
use serde_json::Value;

use super::ApiContext;

/// The service API as the UI calls it.
struct DaemonBackend {
    context: ApiContext,
    runtime: Runtime,
    ui: Arc<super::ui_state::UiServices>,
}

/// The runtime the calls run on: the server's, or (outside one) a runtime of the backend's own.
enum Runtime {
    Handle(tokio::runtime::Handle),
    Own(OnceLock<tokio::runtime::Runtime>),
}

impl Runtime {
    fn block_on<F: std::future::Future>(&self, future: F) -> Option<F::Output> {
        match self {
            Runtime::Handle(handle) => Some(handle.block_on(future)),
            Runtime::Own(cell) => {
                let runtime = match cell.get() {
                    Some(runtime) => runtime,
                    None => {
                        let runtime =
                            tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().ok()?;
                        let _ = cell.set(runtime);
                        cell.get()?
                    }
                };
                Some(runtime.block_on(future))
            }
        }
    }
}

impl DaemonBackend {
    fn assign_native_field(
        tune: &mut openrigcue_model::native_value::LegacyValue,
        path: &[&str],
        value: openrigcue_model::native_value::LegacyValue,
    ) -> Result<(), Failure> {
        let (key, tail) =
            path.split_first().ok_or_else(|| Failure { status: 500, detail: "ValueError: empty tune path".into() })?;
        if tail.is_empty() {
            return tune
                .assign_item(key, value)
                .map_err(|detail| Failure { status: 500, detail: format!("TypeError: {detail}") });
        }
        // Read the intermediate parent through `item_owned` so a live profile Object alias keeps the same
        // child handle; the error class is that of a failed key lookup on the tune.
        let mut parent = tune.item_owned(key).map_err(|detail| Failure {
            status: 500,
            detail: format!("{}: {detail}", if tune.resolved().as_dict().is_some() { "KeyError" } else { "TypeError" }),
        })?;
        Self::assign_native_field(&mut parent, tail, value)?;
        tune.assign_item(key, parent).map_err(|detail| Failure { status: 500, detail: format!("TypeError: {detail}") })
    }

    fn send(&self, method: Method, path: &str, body: Bytes) -> Result<Value, Failure> {
        let (status, _, bytes) = self.raw(method, path, body)?;
        let value: Value =
            if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
        if status.is_success() {
            Ok(value)
        } else {
            // A failure the service wrote as a sentence with values reaches the UI in the selected language;
            // every other detail is passed as it is (the UI looks a fixed text up where it shows one).
            let detail = match value.get("sentence").and_then(Value::as_str) {
                Some(sentence) => self.ui.translate_sentence(sentence),
                None => value
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned()),
            };
            Err(Failure { status: status.as_u16(), detail })
        }
    }

    /// One in-process call: status, `Content-Disposition` file name and body.
    fn raw(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
    ) -> Result<(axum::http::StatusCode, String, Vec<u8>), Failure> {
        let context = self.context.clone();
        let response = self
            .runtime
            .block_on(async move {
                let response = super::direct::call(context, method, path, body).await;
                let status = response.status();
                let file_name = response
                    .headers()
                    .get(header::CONTENT_DISPOSITION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.split_once("filename=\"").map(|(_, rest)| rest.trim_end_matches('"').to_owned()))
                    .unwrap_or_default();
                let bytes = to_bytes(response.into_body(), usize::MAX)
                    .await
                    .map_err(|e| Failure { status: 500, detail: e.to_string() })?;
                Ok::<_, Failure>((status, file_name, bytes.to_vec()))
            })
            .ok_or_else(|| Failure { status: 500, detail: "no runtime".to_owned() })??;
        Ok(response)
    }
}

impl Backend for DaemonBackend {
    fn telemetry_all_channels(&self) -> bool {
        self.context.telemetry.shows_all_channels()
    }

    fn assign_telemetry_plot(&self, channel: &str) -> Result<(), Failure> {
        self.context.state.controls.write().telemetry.plot_channel = Some(channel.into());
        Ok(())
    }

    fn clear_telemetry_history(&self) -> Result<(), Failure> {
        self.context.telemetry.clear_history();
        Ok(())
    }

    fn set_telemetry_reset(&self, reset: bool) -> Result<(), Failure> {
        self.context.state.controls.write().telemetry.reset_min_max = reset;
        Ok(())
    }

    fn reset_telemetry(&self) -> Result<(), Failure> {
        self.context.telemetry.reset_extremes();
        self.set_telemetry_reset(true)
    }

    fn hardware_parameter(&self, name: &str) -> Result<LegacyValue, Failure> {
        match name {
            "intellibox_model" => Ok(self.context.accessories.intellibox_model().into()),
            "intellibox_version" => Ok(self.context.accessories.intellibox_version().into()),
            "belt_model" | "belt_version" | "belt_sn" => {
                let identity = self.context.accessories.belt_identity();
                Ok(match (name, identity) {
                    ("belt_model", Some(identity)) => identity.model.into(),
                    ("belt_version", Some(identity)) => identity.version.into(),
                    ("belt_sn", Some(identity)) => identity.serial.into(),
                    ("belt_model" | "belt_sn", None) => "".into(),
                    _ => (-1).into(),
                })
            }
            "motion_warning" => Ok(self.context.state.motion.read().warning.clone().into()),
            "motion_error" => Ok(self.context.state.motion.read().error.into()),
            "axis_assignment_status" => {
                Ok(self.context.state.motion.read().assignment.as_ref().is_some_and(|a| a.status).into())
            }
            _ => openrigcue_ui::backend::hardware_parameter_from_backend(self, name),
        }
    }

    fn native_tune(&self) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        Ok(self.context.state.profile.read().native_tune())
    }

    fn test_shaker_effect(&self, key: &str, current: &LegacyValue, test: &LegacyValue) -> Result<(), Failure> {
        let effect = openrigcue_effects::shakers::Effect::ALL
            .into_iter()
            .find(|effect| effect.key() == key)
            .ok_or_else(|| Failure {
                status: 500,
                detail: format!("KeyError: {}", openrigcue_model::python_repr::repr_str(key)),
            })?;
        // The listener producers use strings; the service represents
        // 'none' with Option::None. No extra session/soundcard read belongs here.
        let listener = |value: &LegacyValue| match value {
            LegacyValue::Str(value) if value == "none" => Ok(None),
            LegacyValue::Str(value) => Ok(Some(value.clone())),
            _ => Err(Failure { status: 500, detail: "TypeError: listener_running is not a string".into() }),
        };
        crate::shakers::test_effect_with_listeners(
            &self.context.state,
            &self.context.shakers,
            effect,
            listener(current)?,
            listener(test)?,
        )
        .map_err(|error| Failure { status: 500, detail: error.detail() })
    }

    fn test_shaker_speaker(&self, name: &str) -> Result<(), Failure> {
        use openrigcue_effects::shakers::Speaker;
        let speaker = match name {
            "FL" => Speaker::FrontLeft,
            "FR" => Speaker::FrontRight,
            "CE" => Speaker::Center,
            "SUB" => Speaker::Sub,
            "RL" => Speaker::RearLeft,
            "RR" => Speaker::RearRight,
            _ => return Err(Failure { status: 500, detail: format!("KeyError: {name:?}") }),
        };
        crate::shakers::test_sound(&self.context.state, &self.context.shakers, speaker)
            .map_err(|error| Failure { status: 500, detail: format!("{}: {}", error.class, error.message) })
    }

    fn intellitune_samples(&self) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        Ok(openrigcue_model::native_value::LegacyValue::Dict(
            self.context.telemetry.intellitune_snapshot().native_values().clone(),
        ))
    }

    fn clear_intellitune_samples(&self) -> Result<(), Failure> {
        self.context.telemetry.take_intellitune();
        Ok(())
    }

    fn set_intellitune_capture(&self, capture: bool) -> Result<(), Failure> {
        self.context.state.controls.write().telemetry.intellitune_capture = capture;
        Ok(())
    }

    fn telemetry_history(&self) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        Ok(openrigcue_model::native_value::LegacyValue::List(
            self.context.telemetry.history_snapshot().into_iter().collect(),
        ))
    }

    fn accessory_status(&self, name: &str) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        use openrigcue_effects::accessories::port_scan::Device;
        if name == "iflag_firmware" {
            return Ok(self.context.accessories.iflag_version());
        }
        if name == "hud" {
            return Ok(self.context.accessories.hud.status().text().into());
        }
        let (key, port) = name.strip_suffix("_port").map_or((name, false), |key| (key, true));
        let device = match key {
            "intellibox" => Device::Intellibox,
            "belt" => Device::Belt,
            "hapticr" => Device::HaptiCr,
            "sim3d" => Device::Sim3d,
            "iflag" => Device::IFlag,
            "compensation" => Device::MotionCompensation,
            _ => return Err(Failure { status: 500, detail: format!("KeyError: {name:?}") }),
        };
        Ok(openrigcue_model::native_value::LegacyValue::Str(if port {
            self.context.accessories.connection_port(device)
        } else {
            self.context.accessories.connection_status(device)
        }))
    }

    fn control_value(&self, path: &[&str]) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        use openrigcue_model::native_value::LegacyValue;
        if path == ["tests", "belt_homing"] {
            return Ok(LegacyValue::Int(
                crate::accessories::homing_code(self.context.accessories.belt_homing()).into(),
            ));
        }
        let controls = self.context.state.controls.read();
        if path == ["tests", "belt_test_value"] {
            return Ok(controls.belt_test_value());
        }
        if let ["tests", "motion_sliders", key] = path {
            return controls
                .motion_test_value(key)
                .ok_or_else(|| Failure { status: 500, detail: format!("KeyError: {key:?}") });
        }
        let value = match path {
            ["tests", "motion_test"] => i64::from(controls.tests.motion_test),
            ["tests", "iflag_test"] => i64::from(controls.tests.iflag_test),
            ["tests", "haptic_test"] => controls.tests.haptic_test,
            ["tests", "belt_test"] => i64::from(controls.tests.belt_test),
            ["switches", "belt"] => i64::from(controls.switches.belt),
            ["switches", "motion"] => i64::from(controls.switches.motion),
            ["switches", "shakers"] => i64::from(controls.switches.shakers),
            ["switches", "compensation"] => i64::from(controls.switches.compensation),
            ["switches", "sim3d"] => i64::from(controls.switches.sim3d),
            ["switches", "hud"] => i64::from(controls.switches.hud),
            ["switches", "iflag"] => i64::from(controls.switches.iflag),
            ["switches", "haptic"] => i64::from(controls.switches.haptic),
            ["switches", "wind"] => controls.switches.wind.code(),
            ["switches", "wind_center"] => controls.switches.wind_center.code(),
            ["switches", "led"] => controls.switches.led.code(),
            ["switches", "tach"] => controls.switches.tach.code(),
            ["telemetry", "capture"] => return Ok(LegacyValue::Bool(controls.telemetry.capture)),
            ["telemetry", "reset_min_max"] => return Ok(LegacyValue::Bool(controls.telemetry.reset_min_max)),
            ["telemetry", "export"] => return Ok(LegacyValue::Bool(controls.telemetry.export)),
            ["profile_edit"] => return Ok(LegacyValue::Bool(controls.profile_edit.is_some())),
            _ => return Err(Failure { status: 500, detail: format!("KeyError: {path:?}") }),
        };
        Ok(LegacyValue::Int(value))
    }

    fn assign_motion_test_value(
        &self,
        key: &str,
        value: &openrigcue_model::native_value::LegacyValue,
        projection: f64,
    ) -> Result<(), Failure> {
        self.context
            .state
            .controls
            .write()
            .assign_motion_test_value(key, value.clone(), projection)
            .map_err(|detail| Failure { status: 500, detail })
    }

    fn assign_belt_test_value(
        &self,
        value: &openrigcue_model::native_value::LegacyValue,
        projection: f64,
    ) -> Result<(), Failure> {
        self.context.state.controls.write().assign_belt_test_value(value.clone(), projection);
        Ok(())
    }

    fn assign_yaw_limit(&self, key: &str, value: i64) -> Result<(), Failure> {
        super::ui_state::assign_yaw_limit(&self.context, key, value).map_err(|detail| Failure { status: 500, detail })
    }

    fn request_yaw_center(&self) -> Result<(), Failure> {
        self.context.motion.driver_settings().yaw_shared.update(|state| state.homing_requested = true);
        Ok(())
    }

    fn active_profile_game(&self) -> Result<String, Failure> {
        Ok(self.context.state.profile.read().game.clone())
    }

    fn active_profile_vehicle(&self) -> Result<LegacyValue, Failure> {
        Ok(self.context.state.profile.read().vehicle.native_value())
    }

    fn soundcard_refresh_requested(&self) -> Result<bool, Failure> {
        Ok(self.context.shakers.soundcard_refresh_requested())
    }

    fn game_value(&self) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        Ok(openrigcue_model::native_value::LegacyValue::Str(self.context.state.session.read().game_key().to_owned()))
    }

    fn game_name_value(&self, game: &LegacyValue) -> Result<LegacyValue, Failure> {
        let key = openrigcue_ui::backend::game_catalog_key(game)?;
        openrigcue_config::catalog::Catalog::builtin()
            .game(key)
            .map(|game| LegacyValue::Str(game.name.clone()))
            .ok_or_else(|| Failure {
                status: 500,
                detail: format!("KeyError: {}", openrigcue_model::python_repr::repr(game)),
            })
    }

    fn session_value(&self, parameter: &str) -> Result<LegacyValue, Failure> {
        let session = self.context.state.session.read();
        match parameter {
            "listener_running" => Ok(session.listener_key().into()),
            "vehicle_name" => Ok(session.vehicle_value()),
            _ => Err(Failure {
                status: 500,
                detail: format!("KeyError: {}", openrigcue_model::python_repr::repr_str(parameter)),
            }),
        }
    }

    fn previous_vehicle_value(&self) -> Result<LegacyValue, Failure> {
        Ok(self.context.main_loop.previous_vehicle_value())
    }

    fn update_thanos_pending(&self, secondary: bool, replace: bool, entries: &[(String, i64)]) -> Result<(), Failure> {
        super::ui_state::publish_thanos_pending(&self.context, secondary, replace, entries);
        Ok(())
    }

    fn apply_all_tunes(&self, spike: bool, value: &mut dyn FnMut() -> Result<i64, String>) -> Result<(), Failure> {
        super::ui_state::apply_all_tunes_native(&self.context, spike, value)
            .map_err(|detail| Failure { status: 500, detail })
    }

    fn motion_parameter(&self, name: &str) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        use openrigcue_devices::motion::{thanos::protocol::FieldValue, LinkStatus};
        use openrigcue_model::native_value::LegacyValue;
        match name {
            "actuators_position" => {
                use openrigcue_model::native_value::Dict;
                let view = self.context.state.motion.read();
                let mut positions = Dict::new();
                let keys = ["front_left", "front_right", "rear_left", "rear_right"];
                if let Some(travel) = view.travel {
                    for (key, value) in keys.into_iter().zip([
                        travel.front_left,
                        travel.front_right,
                        travel.rear_left,
                        travel.rear_right,
                    ]) {
                        positions.insert(key, LegacyValue::Float(value));
                    }
                } else {
                    let corners = view.actuators.as_ref().map_or([0; 4], |positions| {
                        [positions.front_left, positions.front_right, positions.rear_left, positions.rear_right]
                    });
                    for (key, value) in keys.into_iter().zip(corners) {
                        positions.insert(key, LegacyValue::Int(value));
                    }
                }
                if let Some(actuators) = &view.actuators {
                    for (index, value) in actuators.motors.into_iter().enumerate() {
                        positions.insert(format!("m{index}"), LegacyValue::Int(value));
                    }
                }
                Ok(LegacyValue::Dict(positions))
            }
            "actuators" => {
                use openrigcue_model::native_value::{Dict, Key};
                let assignment = self.context.state.motion.read().assignment.clone();
                let mut actuators = Dict::new();
                for actuator in assignment.into_iter().flat_map(|assignment| assignment.actuators) {
                    let key = i64::try_from(actuator.key)
                        .map_err(|_| Failure { status: 500, detail: "unsupported actuator-view key width".into() })?;
                    let mut fields = Dict::new();
                    fields.insert("axis", LegacyValue::Str(actuator.axis));
                    fields.insert("com_port", LegacyValue::Str(actuator.com_port));
                    fields.insert("sn", LegacyValue::Str(actuator.sn));
                    actuators.insert(Key::Int(key), LegacyValue::Dict(fields));
                }
                Ok(LegacyValue::Dict(actuators))
            }
            "prs200_snc6_tl_com_port" => {
                Ok(LegacyValue::Str(self.context.motion.driver_settings().prs200_tl_port.get()))
            }
            "motion_status" => Ok(LegacyValue::Str(
                match self.context.motion.link_state().status {
                    LinkStatus::Connected => "Connected",
                    LinkStatus::NotConnected => "Not connected",
                    LinkStatus::NotConnectedAfterWriteError => "Not Connected",
                }
                .into(),
            )),
            "motion_com_port" => {
                use openrigcue_model::native_value::{Dict, Key};
                let mut ports = Dict::new();
                for (slot, port) in self.context.motion.link_state().ports {
                    let slot = i64::try_from(slot)
                        .map_err(|_| Failure { status: 500, detail: "unsupported motion-port key width".into() })?;
                    ports.insert(Key::Int(slot), LegacyValue::Str(port));
                }
                Ok(LegacyValue::Dict(ports))
            }
            "thanos_controller" => {
                Ok(LegacyValue::Str(self.context.motion.driver_settings().thanos_shared.get().controller.controller))
            }
            "thanos_firmware" => {
                Ok(LegacyValue::Str(self.context.motion.driver_settings().thanos_shared.get().controller.firmware))
            }
            name @ ("thanos_config" | "thanos_config2") => {
                let controller = self.context.motion.driver_settings().thanos_shared.get().controller;
                let config =
                    if name == "thanos_config" { &controller.main_config } else { &controller.secondary_config };
                Ok(LegacyValue::Dict(
                    config
                        .entries()
                        .iter()
                        .map(|(field, value)| {
                            (
                                field.name().into(),
                                match value {
                                    FieldValue::Int(value) => LegacyValue::Int(*value),
                                    FieldValue::Text(value) => LegacyValue::Str(value.clone()),
                                },
                            )
                        })
                        .collect(),
                ))
            }
            "Yaw VR" => {
                use openrigcue_model::native_value::Dict;
                let shared = self.context.motion.driver_settings().yaw_shared.get();
                let mut values = Dict::new();
                values.insert("centered", LegacyValue::Bool(shared.centered));
                values.insert("device_model", LegacyValue::Str(shared.device_model));
                if let Some(limits) = shared.limits {
                    for (key, value) in [
                        ("power", limits.power),
                        ("pitch_fwd_limit", limits.pitch_forward),
                        ("pitch_bwd_limit", limits.pitch_backward),
                        ("roll_limit", limits.roll),
                        ("yaw_limit", limits.yaw),
                        ("yaw_limit_enabled", limits.yaw_limit_enabled),
                        ("vibration_enabled", limits.vibration_enabled),
                    ] {
                        values.insert(key, LegacyValue::Int(value));
                    }
                }
                Ok(LegacyValue::Dict(values))
            }
            "yaw_limit_enabled" => self
                .context
                .motion
                .driver_settings()
                .yaw_shared
                .get()
                .limits
                .map(|limits| LegacyValue::Int(limits.yaw_limit_enabled))
                .ok_or_else(|| Failure { status: 500, detail: "KeyError: 'yaw_limit_enabled'".into() }),
            _ => Err(Failure { status: 500, detail: format!("KeyError: {name:?}") }),
        }
    }

    fn motion_sensor_value(&self, key: &str) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        self.context.accessories.motion_sensor_value(key).ok_or_else(|| Failure {
            status: 500,
            detail: format!("KeyError: {}", openrigcue_model::python_repr::repr_str(key)),
        })
    }

    fn native_config_section(&self, section: &str, _keys: &[&str]) -> Result<LegacyValue, Failure> {
        Ok(self
            .context
            .state
            .config
            .read()
            .settings()
            .native_snapshot()
            .get(section)
            .cloned()
            .unwrap_or_else(|| LegacyValue::Dict(Default::default())))
    }

    fn native_setting(&self, section: &str, key: &str) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        // The live rooted settings tree: section and key reads return the same
        // section/value handles across revisions, so a page that keeps one
        // settings alias sees accepted assignments through it.
        // `item_owned` retains child identity and holds no
        // lock after it returns.
        let section_value = self
            .context
            .state
            .config
            .read()
            .native_settings_tree()
            .item_owned(section)
            .map_err(|detail| Failure { status: 500, detail: format!("KeyError: {detail}") })?;
        section_value.item_owned(key).map_err(|detail| Failure {
            status: 500,
            detail: format!(
                "{}: {detail}",
                if section_value.resolved().as_dict().is_some() { "KeyError" } else { "TypeError" }
            ),
        })
    }

    fn assign_mc_value(
        &self,
        effect: &str,
        field: &str,
        value: &openrigcue_model::native_value::LegacyValue,
    ) -> Result<(), Failure> {
        use openrigcue_model::native_value::LegacyValue;
        let mut store = self.context.state.config.write();
        let mut root = LegacyValue::Dict(store.settings().native_snapshot());
        Self::assign_native_field(&mut root, &["MOTION_COMPENSATION", effect, field], value.clone())?;
        let LegacyValue::Dict(raw) = root else { unreachable!("settings root remains a dictionary") };
        store.update(|configuration| configuration.settings.edit_native(|settings| *settings = raw));
        Ok(())
    }

    fn confirm_game_port(
        &self,
        entry: &str,
        port: i64,
        forwarding: &openrigcue_model::native_value::LegacyValue,
    ) -> Result<bool, Failure> {
        self.runtime
            .block_on(super::routes::confirm_game_port(&self.context, entry, port, forwarding.clone(), false))
            .ok_or_else(|| Failure { status: 500, detail: "no runtime".into() })?
            .map_err(|error| Failure { status: error.status.as_u16(), detail: error.detail })
    }

    fn assign_motion_setting(
        &self,
        key: &str,
        value: &openrigcue_model::native_value::LegacyValue,
    ) -> Result<(), Failure> {
        self.assign_setting_value("MOTIONBOX", key, value)
    }

    fn assign_setting_value(
        &self,
        section_name: &str,
        key: &str,
        value: &openrigcue_model::native_value::LegacyValue,
    ) -> Result<(), Failure> {
        use openrigcue_model::native_value::LegacyValue;
        let mut store = self.context.state.config.write();
        let settings = LegacyValue::Dict(store.settings().native_snapshot());
        let mut section = settings
            .item(section_name)
            .map_err(|detail| Failure { status: 500, detail: format!("KeyError: {detail}") })?
            .clone();
        section
            .assign_item(key, value.clone())
            .map_err(|detail| Failure { status: 500, detail: format!("TypeError: {detail}") })?;
        store.update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.insert(section_name, section);
            })
        });
        Ok(())
    }

    fn reload_led_colors(&self) -> Result<(), Failure> {
        use openrigcue_model::native_value::{Dict, LegacyValue};
        // The LED model: `LED.model`, `RGB` when the section or key is missing. This is an explicit callback hook;
        // ordinary settings assignments must leave the cached named colours untouched.
        let settings = self.context.state.config.read().settings().native_snapshot();
        let empty = LegacyValue::Dict(Dict::new());
        let led = settings.get("LED").unwrap_or(&empty);
        let fields = led.as_dict().ok_or_else(|| Failure {
            status: 500,
            detail: format!("AttributeError: '{}' object has no attribute 'get'", led.type_name()),
        })?;
        let default = LegacyValue::from("RGB");
        let model = fields.get("model").unwrap_or(&default);
        self.context.accessories.load_hex_colors(model.as_str().unwrap_or(""));
        Ok(())
    }

    fn assign_shaker_value(
        &self,
        table: &str,
        key: &str,
        value: &openrigcue_model::native_value::LegacyValue,
    ) -> Result<(), Failure> {
        self.assign_tune_value(&[table, key], value)?;
        self.context
            .shakers
            .load_channel_groups(&self.context.state)
            .map_err(|error| Failure { status: 500, detail: format!("{}: {}", error.class, error.message) })
    }

    fn assign_tune_value(
        &self,
        path: &[&str],
        value: &openrigcue_model::native_value::LegacyValue,
    ) -> Result<(), Failure> {
        let mut profile = self.context.state.profile.write();
        let mut tune = profile.native_tune();
        Self::assign_native_field(&mut tune, path, value.clone())?;
        profile.replace_native_tune(tune);
        profile.generation += 1;
        Ok(())
    }

    fn location_value(&self) -> openrigcue_model::native_value::LegacyValue {
        self.context.state.session.read().location_value()
    }

    fn clear_profile_edit(&self) -> Result<bool, Failure> {
        self.runtime
            .block_on(super::ui_state::clear_edit_native(&self.context))
            .ok_or_else(|| Failure { status: 500, detail: "no runtime".into() })?
            .map_err(|error| Failure { status: error.status.as_u16(), detail: error.detail })
    }

    fn begin_profile_edit(
        &self,
        game: &str,
        vehicle: &openrigcue_model::native_value::LegacyValue,
    ) -> Result<(), Failure> {
        self.runtime
            .block_on(super::ui_state::begin_edit_native(&self.context, game.to_owned(), vehicle.clone()))
            .ok_or_else(|| Failure { status: 500, detail: "no runtime".into() })?
            .map_err(|error| Failure { status: error.status.as_u16(), detail: error.detail })
    }

    fn save_profile(&self, vehicle: Option<&str>) -> Result<(), Failure> {
        let game = self.context.state.session.read().game_key().to_owned();
        let profile = self.context.state.profile.snapshot();
        let target = match vehicle {
            Some(vehicle) if !vehicle.is_empty() => openrigcue_config::profiles::SaveTarget::Vehicle(vehicle.into()),
            _ => openrigcue_config::profiles::SaveTarget::Game,
        };
        super::routes::save_profile_target(&self.context, &game, target, profile)
            .map(|_| ())
            .map_err(|error| Failure { status: error.status.as_u16(), detail: error.detail })
    }

    fn store_shaker_effects(&self) -> Result<(), Failure> {
        super::ui_state::store_shaker_effects(&self.context)
            .map_err(|error| Failure { status: error.status.as_u16(), detail: error.detail })
    }

    fn confirm_profile_import(&self, file: &openrigcue_config::profiles::ProfileFile) -> Result<(), Failure> {
        self.runtime
            .block_on(super::routes::confirm_profile_import(&self.context, file))
            .ok_or_else(|| Failure { status: 500, detail: "no runtime".into() })?
            .map(|_| ())
            .map_err(|error| Failure { status: error.status.as_u16(), detail: error.detail })
    }

    fn profile_row(&self, game: &str) -> Result<Option<openrigcue_model::native_value::LegacyValue>, Failure> {
        self.context
            .state
            .config
            .read()
            .profile_row(game)
            .map_err(|error| Failure { status: 500, detail: error.native_detail() })
    }

    fn profile_export_at(
        &self,
        game: &str,
        vehicle: &openrigcue_model::native_value::LegacyValue,
        location: Option<&openrigcue_model::native_value::LegacyValue>,
    ) -> Result<(Vec<u8>, String), Failure> {
        use openrigcue_config::profiles::{export_native_profile_from_lookup, ExportMetadata, ProfileError};
        let store = self.context.state.config.read();
        let settings = store.settings();
        let meta = ExportMetadata {
            motion_model: &settings.motion.model,
            shaker_layout: &settings.shakers.speaker_layout,
            builtin_layouts: &openrigcue_config::catalog::registry::Registry::builtin().builtin_shaker_layouts,
        };
        let (name, bytes) = export_native_profile_from_lookup(
            |key| store.profile_row(key).map_err(|error| ProfileError::Native(error.native_detail())),
            openrigcue_config::catalog::Catalog::builtin(),
            game,
            vehicle,
            location,
            &meta,
        )
        .map_err(|error| Failure { status: 500, detail: error.to_string() })?;
        Ok((bytes, name))
    }

    fn metadata_keys(&self, table: &str) -> Result<openrigcue_ui::backend::MetadataKeys, Failure> {
        let keys = self
            .context
            .state
            .config
            .read()
            .metadata_keys(table)
            .map_err(|error| Failure { status: 500, detail: error.native_detail() })?;
        Ok(Box::new(keys.map(|key| key.map_err(|error| Failure { status: 500, detail: error.native_detail() }))))
    }

    fn shaker_effects(&self) -> Result<openrigcue_model::native_value::LegacyValue, Failure> {
        Ok(self.context.state.shaker_layout.read().value.clone())
    }

    fn apply_shaker_soundcard(&self) -> Result<(), Failure> {
        self.context
            .shakers
            .set_soundcard(&self.context.state)
            .map_err(|error| Failure { status: 409, detail: self.ui.translate_sentence(&error.message()) })
    }

    fn confirm_shaker_levels(
        &self,
        speaker: Option<&str>,
        levels: &openrigcue_model::native_value::LegacyValue,
    ) -> Result<(), Failure> {
        crate::shaker_layout::confirm_levels(&self.context.state, speaker, levels)
            .map_err(|detail| Failure { status: 500, detail })?;
        let settings = crate::shakers::shaker_settings(&self.context.state);
        let mut shakers = self.context.shakers.lock();
        shakers.system.update_settings(settings);
        shakers
            .system
            .reload_levels()
            .map_err(|error| Failure { status: 500, detail: format!("{}: {}", error.class, error.message) })
    }

    fn load_shaker_effects(&self) -> Result<(), Failure> {
        crate::shaker_layout::load(&self.context.state).map_err(|detail| Failure { status: 500, detail })
    }

    fn assign_shaker_profile(&self, name: &str) -> Result<Value, Failure> {
        crate::shaker_layout::assign_profile(&self.context.state, name)
            .map_err(|detail| Failure { status: 500, detail })
    }

    fn copy_shaker_profile(&self, source: &str, name: &str) -> Result<(), Failure> {
        crate::shaker_layout::copy(&self.context.state, source, name).map_err(|detail| Failure { status: 500, detail })
    }

    fn delete_shaker_profile(&self, name: &str) -> Result<(), Failure> {
        crate::shaker_layout::remove(&self.context.state, name).map_err(|detail| Failure { status: 500, detail })
    }

    fn recheck_shakers_surround(&self) -> Result<(), Failure> {
        self.context.shakers.trigger_surround_recheck(&self.context.state);
        Ok(())
    }

    fn launcher_flag(&self, key: &str) -> Result<Option<openrigcue_model::native_value::LegacyValue>, Failure> {
        self.context
            .state
            .config
            .read()
            .launcher_flag(key)
            .map_err(|error| Failure { status: 500, detail: error.to_string() })
    }

    fn write_launcher_flag(&self, key: &str, enabled: bool) -> Result<(), Failure> {
        self.context
            .state
            .config
            .write()
            .write_launcher_flag(key, enabled)
            .map_err(|error| Failure { status: 500, detail: error.to_string() })?;
        self.context.state.announce(crate::state::ServiceEvent::Settings);
        Ok(())
    }

    fn metadata_row(
        &self,
        table: &str,
        key: &str,
    ) -> Result<Option<openrigcue_config::import::value::LegacyValue>, Failure> {
        self.context
            .state
            .config
            .read()
            .metadata_row(table, key)
            .map_err(|error| Failure { status: 500, detail: error.native_detail() })
    }

    fn metadata_contains(&self, table: &str, key: &str) -> Result<bool, Failure> {
        self.context
            .state
            .config
            .read()
            .metadata_contains(table, key)
            .map_err(|error| Failure { status: 500, detail: error.native_detail() })
    }

    fn metadata_rows(&self, table: &str) -> Result<openrigcue_ui::backend::MetadataRows, Failure> {
        let rows = self
            .context
            .state
            .config
            .read()
            .metadata_rows(table)
            .map_err(|error| Failure { status: 500, detail: error.native_detail() })?;
        Ok(Box::new(rows.map(|row| row.map_err(|error| Failure { status: 500, detail: error.native_detail() }))))
    }

    fn assign_app_setting(&self, key: &str, value: Value) -> Result<Value, Failure> {
        use openrigcue_model::native_value::LegacyValue;
        if !matches!(
            key,
            "window_width"
                | "window_height"
                | "pixel_ratio"
                | "startup_page"
                | "x_button_exit"
                | "start_minimized"
                | "voice_notifications"
                | "shift_sound"
                | "log_level"
                | "measurement_unit"
                | "theme"
        ) {
            return Err(Failure { status: 400, detail: "unknown App setting".into() });
        }
        let mut store = self.context.state.config.write();
        let mut raw = store.settings().native_snapshot();
        let app = raw.get_mut("APP").ok_or_else(|| Failure { status: 500, detail: "KeyError: 'APP'".into() })?;
        app.assign_item(key, LegacyValue::from(value.clone()))
            .map_err(|error| Failure { status: 500, detail: format!("TypeError: {error}") })?;
        store.update(|config| {
            config.settings.edit_native(|raw| {
                raw.get_mut("APP")
                    .expect("validated APP")
                    .assign_item(key, LegacyValue::from(value))
                    .expect("validated parent");
            })
        });
        let view = store.settings().ui_view().map_err(|error| Failure { status: 500, detail: error.to_string() });
        drop(store);
        if key == "log_level" {
            super::ui_state::apply_log_level(&self.context);
        }
        self.context.state.announce(crate::state::ServiceEvent::Settings);
        view
    }

    fn close_opened_files(&self) {
        self.context.state.config.write().close_opened_files();
    }

    fn capture_window_geometry(&self) {
        self.ui.capture_window_geometry(&self.context.state);
    }

    fn synchronize_profile(&self) -> Result<(), Failure> {
        self.context
            .main_loop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .synchronize_profile(&self.context)
            .map_err(|e| Failure { status: 500, detail: e.to_string() })
    }

    fn profile_reload_needed(&self) -> Result<Option<bool>, Failure> {
        Ok(Some(
            self.context
                .main_loop
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .profile_reload_needed(&self.context),
        ))
    }

    fn load_current_profile(&self) -> Result<(), Failure> {
        self.context
            .main_loop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .load_current_profile(&self.context)
            .map_err(|error| Failure { status: 500, detail: error.to_string() })
    }

    fn hardware_notification(&self, notification: HardwareNotification) -> Result<(), Failure> {
        let mut notifications = self.context.state.notifications.write();
        let before = notifications.clone();
        match notification {
            HardwareNotification::YawVr { needs_center } => notifications.yaw_vr_widget(needs_center),
            HardwareNotification::IflagFirmware => notifications.iflag_firmware_widget(),
            HardwareNotification::Belt { needs_calibration } => notifications.belt_widget(needs_calibration),
        }
        if *notifications != before {
            self.context.state.announce(crate::state::ServiceEvent::Notifications);
        }
        Ok(())
    }

    fn hardware_notification_state(&self) -> HardwareNotificationState {
        self.context.state.notifications.read().hardware_widget_state()
    }

    fn finish_main_loop(&self) -> Result<(), Failure> {
        self.context
            .main_loop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finish_ui_pass(&self.context)
            .map_err(|e| Failure { status: 500, detail: e.to_string() })
    }

    fn remember_actual_vehicle(&self) -> Result<(), Failure> {
        self.context
            .main_loop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remember_actual_vehicle(&self.context);
        Ok(())
    }

    fn check_game_config(&self) -> Result<(), Failure> {
        self.context
            .main_loop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .check_current_game_config(&self.context)
            .map_err(|error| Failure { status: 500, detail: error.to_string() })
    }

    fn assign_belt_switch(&self, enabled: bool) -> Result<(), Failure> {
        self.context.state.controls.write().switches.belt = enabled;
        Ok(())
    }

    fn clear_remembered_vehicle(&self) -> Result<(), Failure> {
        self.context.main_loop.lock().unwrap_or_else(std::sync::PoisonError::into_inner).forget_vehicle();
        Ok(())
    }

    fn remember_profile_inputs(&self) -> Result<(), Failure> {
        self.context
            .main_loop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remember_profile_inputs(&self.context);
        Ok(())
    }

    fn ui_actions(&self) -> Vec<String> {
        self.context.hotkeys.ui_actions()
    }

    fn clear_ui_actions(&self) {
        // Recenter belongs to the VR compensation actions, alongside the axis actions.
        if self.context.hotkeys.ui_actions().iter().any(|a| a == "motion_compensation_recenter") {
            let raw = crate::compensation::RawSources {
                accessories: &self.context.accessories,
                yaw_vr: &self.context.motion.driver_settings().yaw_shared,
            };
            crate::compensation::run_shortcut(
                &self.context.state,
                raw,
                &self.context.compensation,
                "motion_compensation_recenter",
            );
        }
        self.context.hotkeys.clear_ui_actions();
    }

    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        let body = match body {
            Some(value) => Bytes::from(serde_json::to_vec(&value).unwrap_or_default()),
            None => Bytes::new(),
        };
        self.send(method, path, body)
    }

    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, Failure> {
        self.send(Method::Post, path, Bytes::from(bytes))
    }

    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        let (status, file_name, bytes) = self.raw(Method::Get, path, Bytes::new())?;
        if status.is_success() {
            Ok((bytes, file_name))
        } else {
            Err(Failure { status: status.as_u16(), detail: String::from_utf8_lossy(&bytes).into_owned() })
        }
    }

    fn translate(&self, text: &str) -> String {
        self.ui.translate(text)
    }

    fn excluded_strings(&self) -> Vec<String> {
        crate::i18n::excluded_strings()
    }
}

/// The UI routes: the page and websocket at `/`, the style sheet. Constructing the host starts the app;
/// its main loop does not depend on a page request or a connected browser.
pub fn router(context: ApiContext, stop: openrigcue_ui::host::ServerStop) -> (Router, UiHost) {
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(handle) => Runtime::Handle(handle),
        Err(_) => Runtime::Own(OnceLock::new()),
    };
    let backend = DaemonBackend { runtime, ui: context.ui.clone(), context };
    let host = UiHost::with_server_stop(move || openrigcue_ui::app::MyApp::new(Arc::new(backend)), stop);
    (openrigcue_ui::router(host.clone()), host)
}

#[cfg(test)]
mod tests {
    use openrigcue_ui::backend::BackendExt;
    use openrigcue_ui::host::UiApp;

    use super::*;

    /// An independent plain copy of a stored value for a fixture to edit before installing it: every `Object` alias
    /// is read through `resolved()` and copied, so an edit never reaches the live profile or a later iteration's
    /// copy (as the plain values these fixtures were written for behaved).
    fn detached(value: &LegacyValue) -> LegacyValue {
        match value.resolved() {
            LegacyValue::Dict(dict) => {
                LegacyValue::Dict(dict.iter().map(|(key, value)| (key.clone(), detached(value))).collect())
            }
            LegacyValue::List(items) => LegacyValue::List(items.iter().map(detached).collect()),
            LegacyValue::Tuple(items) => LegacyValue::Tuple(items.iter().map(detached).collect()),
            other => other,
        }
    }

    /// The daemon's backend on the harness's service, with a runtime of its own.
    fn harness_backend(h: &super::super::tests::Harness) -> Arc<DaemonBackend> {
        Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        })
    }

    /// The app on `backend` after the first page load (the interface mounted, the start threads taken), with
    /// `screen` open.
    fn mounted_app(backend: Arc<DaemonBackend>, screen: openrigcue_ui::ui::Screen) -> openrigcue_ui::app::MyApp {
        let mut app = openrigcue_ui::app::MyApp::new(backend);
        let body = app.gui.create("body", "BODY");
        let root = app.main(body);
        app.gui.set_root(root);
        openrigcue_ui::remi::gui::dispatch(&mut app, body, "onload", &Default::default());
        drop(app.take_threads());
        assert!(app.ui.mounted, "the interface is mounted at the first page load");
        openrigcue_ui::ui::navigate(&mut app, screen, 0);
        app
    }

    #[test]
    fn joystick_shortcut_editor_captures_and_confirms_through_the_same_store() {
        use openrigcue_ui::ui::dialogs::Dialog;
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Joystick);
        assert!(openrigcue_ui::ui::screens::main(&app).contains("data-arg=\"Joystick:@header:0\""), "Shortcuts");
        // Without stored bindings the editor opens with empty fields and the capture runs.
        openrigcue_ui::ui::dispatch(&mut app, "action", "Joystick:@header:0", "").unwrap();
        assert!(h.context.hotkeys.capturing(), "the editor starts the service's capture");
        let Some(Dialog::Shortcut(editor)) = &app.ui.dialog else { panic!("the shortcut editor is open") };
        assert_eq!(editor.actions[editor.selected].0, "joystick_switch");
        assert_eq!((editor.keys.as_str(), editor.joystick.as_str()), ("", "None"));
        openrigcue_ui::ui::dispatch(&mut app, "dialog-close", "", "").unwrap();
        assert!(!h.context.hotkeys.capturing(), "closing the editor ends the capture");
        assert!(app.ui.dialog.is_none());
        backend.assign_setting_value("SHORTCUTS", "joystick_switch_keys", &"old".into()).unwrap();
        backend.assign_setting_value("SHORTCUTS", "joystick_switch_btns", &"".into()).unwrap();
        openrigcue_ui::ui::dispatch(&mut app, "action", "Joystick:@header:0", "").unwrap();
        assert!(h.context.hotkeys.capturing());
        let Some(Dialog::Shortcut(editor)) = app.ui.dialog.as_mut() else { panic!("the shortcut editor is open") };
        assert_eq!(editor.keys, "old", "the stored binding shows");
        // A captured combination fills the keyboard field.
        editor.keys = "ctrl+alt+j".into();
        openrigcue_ui::ui::dispatch(&mut app, "dialog-field", "joystick", "0").unwrap();
        openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert_eq!(backend.native_setting("SHORTCUTS", "joystick_switch_keys").unwrap(), "ctrl+alt+j".into());
        assert_eq!(backend.native_setting("SHORTCUTS", "joystick_switch_btns").unwrap(), "".into());
        assert!(!h.context.hotkeys.capturing(), "confirming ends the capture");
        assert!(app.ui.dialog.is_none());
        drop(app.take_threads());
    }

    #[test]
    fn display_all_telemetry_native_settings_seed_the_first_table_and_stay_latched() {
        use crate::runtime::Shared;
        use openrigcue_sources::derive::Dispatcher;
        let mut h = super::super::tests::harness();
        h.context.state.config.write().update(|configuration| {
            configuration.settings = configuration
                .settings
                .apply_ui_patch(&serde_json::json!({"app":{"show_all_telemetry_channels":true}}))
                .unwrap();
        });
        h.context.telemetry = crate::telemetry::TelemetryHandle::for_settings(
            Dispatcher::new(Vec::new()),
            h.context.state.config.read().settings(),
        );
        assert!(h.context.telemetry.shows_all_channels());
        h.context.state.config.write().update(|configuration| {
            configuration.settings.app.show_all_telemetry_channels = false;
        });
        let parts = crate::telemetry::TelemetryParts {
            dispatcher: h.context.telemetry.clone(),
            snapshot: Shared::default(),
            context: Shared::default(),
            outputs: Shared::default(),
            joystick_profile: Shared::new("Default".into()),
        };
        crate::telemetry::main_loop_tick(&h.context.state, &parts, &crate::telemetry::CatalogDirectory::default());
        assert!(h.context.telemetry.shows_all_channels(), "read(false) never restores hidden flags");
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Telemetry);
        let hidden = openrigcue_ui::catalog::channels().into_iter().find(|c| c.name == "max_rpm").unwrap();
        assert!(!hidden.display, "max_rpm is listed only with every channel shown");
        assert!(app.ui.devices.telemetry.all_channels);
        assert!(
            openrigcue_ui::ui::screens::main(&app).contains("data-channel=\"max_rpm\""),
            "the startup display flag reaches the screen"
        );
        let prior = openrigcue_ui::ui::screens::telemetry::Reading {
            value: "1".into(),
            min: "prior".into(),
            max: "prior".into(),
        };
        app.ui.devices.telemetry.readings.insert("max_rpm".into(), prior);
        backend.reset_telemetry().unwrap();
        openrigcue_ui::ui::screens::telemetry::rules(&mut app).unwrap();
        assert_eq!(app.ui.devices.telemetry.readings["max_rpm"].min, "");
        assert_eq!(app.ui.devices.telemetry.readings["max_rpm"].max, "");
        assert!(backend.telemetry_all_channels());
        assert!(app.ui.devices.telemetry.all_channels);
        drop(app.take_threads());
    }

    #[test]
    fn shared_telemetry_reset_hotkey_sets_and_capture_clears_native_flag() {
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        assert_eq!(backend.control_value(&["telemetry", "reset_min_max"]).unwrap(), LegacyValue::Bool(false));
        crate::hotkeys::actions::run(&h.context, h._tmp.path(), "telemetry_reset");
        assert_eq!(backend.control_value(&["telemetry", "reset_min_max"]).unwrap(), LegacyValue::Bool(true));
        crate::hotkeys::actions::run(&h.context, h._tmp.path(), "telemetry_capture");
        assert_eq!(backend.control_value(&["telemetry", "reset_min_max"]).unwrap(), LegacyValue::Bool(false));
        assert_eq!(backend.control_value(&["telemetry", "capture"]).unwrap(), LegacyValue::Bool(true));
    }

    #[test]
    fn shared_telemetry_reset_hotkey_reaches_ui_cells_without_running_jobs() {
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Telemetry);
        assert!(openrigcue_ui::ui::screens::main(&app).contains("data-channel=\"pitch\""));
        let old = openrigcue_ui::ui::screens::telemetry::Reading {
            value: "1".into(),
            min: "old min".into(),
            max: "old max".into(),
        };
        app.ui.devices.telemetry.readings.insert("pitch".into(), old);
        crate::hotkeys::actions::run(&h.context, h._tmp.path(), "telemetry_reset");
        assert_eq!(backend.control_value(&["telemetry", "reset_min_max"]).unwrap(), LegacyValue::Bool(true));
        // The main loop's pass of the screen's rules, without the app's threads.
        openrigcue_ui::ui::screens::telemetry::rules(&mut app).unwrap();
        assert_eq!(app.ui.devices.telemetry.readings["pitch"].min, "");
        assert_eq!(app.ui.devices.telemetry.readings["pitch"].max, "");
        assert!(!openrigcue_ui::ui::screens::main(&app).contains("old min"));
        assert_eq!(backend.control_value(&["telemetry", "reset_min_max"]).unwrap(), LegacyValue::Bool(false));
        drop(app.take_threads());
    }

    #[test]
    fn shared_telemetry_reset_initial_game_stop_survives_ui_construction() {
        use crate::runtime::Shared;
        let h = super::super::tests::harness();
        let parts = crate::telemetry::TelemetryParts {
            dispatcher: h.context.telemetry.clone(),
            snapshot: Shared::default(),
            context: Shared::default(),
            outputs: Shared::default(),
            joystick_profile: Shared::new("Default".into()),
        };
        crate::telemetry::main_loop_tick(&h.context.state, &parts, &crate::telemetry::CatalogDirectory::default());
        let backend = harness_backend(&h);
        assert_eq!(backend.control_value(&["telemetry", "reset_min_max"]).unwrap(), LegacyValue::Bool(true));
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Home);
        assert_eq!(
            backend.control_value(&["telemetry", "reset_min_max"]).unwrap(),
            LegacyValue::Bool(true),
            "building the interface must not discard the earlier reset"
        );
        let prior = openrigcue_ui::ui::screens::telemetry::Reading {
            value: "1".into(),
            min: "prior".into(),
            max: "prior".into(),
        };
        app.ui.devices.telemetry.readings.insert("rpm".into(), prior);
        // Opening the screen reads the pass at once: capture is off, so the reset clears the extremes.
        openrigcue_ui::ui::navigate(&mut app, openrigcue_ui::ui::Screen::Telemetry, 0);
        assert_eq!(backend.control_value(&["telemetry", "reset_min_max"]).unwrap(), LegacyValue::Bool(false));
        assert_eq!(app.ui.devices.telemetry.readings["rpm"].min, "");
        assert_eq!(app.ui.devices.telemetry.readings["rpm"].max, "");
        drop(app.take_threads());
    }

    #[test]
    fn shared_telemetry_reset_button_and_capture_share_the_runtime_flag() {
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Telemetry);
        assert!(openrigcue_ui::ui::screens::main(&app).contains("data-arg=\"Telemetry:capture_telemetry:0\""), "Reset");
        openrigcue_ui::ui::dispatch(&mut app, "action", "Telemetry:capture_telemetry:0", "").unwrap();
        assert_eq!(backend.control_value(&["telemetry", "reset_min_max"]).unwrap(), LegacyValue::Bool(true));
        openrigcue_ui::ui::dispatch(&mut app, "set", "Telemetry:capture_telemetry", "1").unwrap();
        assert_eq!(backend.control_value(&["telemetry", "reset_min_max"]).unwrap(), LegacyValue::Bool(false));
        assert_eq!(backend.control_value(&["telemetry", "capture"]).unwrap(), LegacyValue::Bool(true));
        drop(app.take_threads());
    }

    #[test]
    fn selected_telemetry_row_clears_live_history_even_for_the_same_channel() {
        use openrigcue_sources::derive::{Dispatcher, Listener, Rate};
        struct Frame;
        impl Listener for Frame {
            fn id(&self) -> &str {
                "api"
            }
            fn is_running(&mut self) -> bool {
                true
            }
            fn receive(&mut self) {}
            fn channel(&self, name: &str) -> Option<f64> {
                Some(match name {
                    "speed" => 20.0,
                    "rpm" => 5000.0,
                    "max_rpm" => 8000.0,
                    _ => 0.0,
                })
            }
            fn rate(&self, _: Rate) -> Option<f64> {
                Some(0.0)
            }
            fn vehicle(&self) -> Option<String> {
                None
            }
            fn location(&self) -> Option<String> {
                None
            }
        }
        let mut h = super::super::tests::harness();
        h.context.telemetry = crate::telemetry::TelemetryHandle::new(Dispatcher::new(vec![Box::new(Frame)]));
        h.context.state.controls.write().telemetry.plot_channel = Some("rpm".into());
        let parts = crate::telemetry::TelemetryParts {
            dispatcher: h.context.telemetry.clone(),
            snapshot: Default::default(),
            context: Default::default(),
            outputs: Default::default(),
            joystick_profile: crate::runtime::Shared::new("Default".into()),
        };
        crate::telemetry::main_loop_tick(&h.context.state, &parts, &crate::telemetry::CatalogDirectory::default());
        assert!(!h.context.telemetry.history_snapshot().is_empty());
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend, openrigcue_ui::ui::Screen::Telemetry);
        assert!(openrigcue_ui::ui::screens::main(&app).contains("data-arg=\"Telemetry:plot\""), "the Plot choice");
        assert!(!h.context.telemetry.history_snapshot().is_empty());
        openrigcue_ui::ui::dispatch(&mut app, "set", "Telemetry:plot", "rpm").unwrap();
        assert!(h.context.telemetry.history_snapshot().is_empty());
        assert_eq!(app.ui.devices.telemetry.history, None);
        assert_eq!(h.context.state.controls.read().telemetry.plot_channel.as_deref(), Some("rpm"));
        drop(app.take_threads());
    }

    #[test]
    fn led_segment_edits_preserve_fresh_siblings_and_fail_without_the_led_section() {
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend, openrigcue_ui::ui::Screen::Led);
        assert!(openrigcue_ui::ui::screens::main(&app).contains(">Segment 1</option>"), "segment 1's settings group");
        h.context.state.config.write().update(|config| {
            config.settings.edit_native(|raw| {
                let led = raw.get_mut("LED").unwrap().as_dict_mut().unwrap();
                led.insert("segment2_start", 77.into());
                led.insert("future_field", LegacyValue::Bytes(b"retain".to_vec()));
            })
        });
        // "Off", the last of the segment types.
        openrigcue_ui::ui::dispatch(&mut app, "set", "LED:segment1dropdown", "9").unwrap();
        let native = h.context.state.config.read().settings().native_snapshot();
        assert_eq!(native.get("LED").unwrap().item_owned("segment1_type").unwrap(), LegacyValue::from("O"));
        assert_eq!(native.get("LED").unwrap().item_owned("segment2_start").unwrap(), LegacyValue::Int(77));
        assert_eq!(
            native.get("LED").unwrap().item_owned("future_field").unwrap(),
            LegacyValue::Bytes(b"retain".to_vec())
        );
        h.context.state.config.write().update(|config| {
            config.settings.edit_native(|raw| {
                raw.insert("LED", LegacyValue::None);
            })
        });
        openrigcue_ui::ui::dispatch(&mut app, "action", "LED:segment1color:", "").unwrap();
        openrigcue_ui::ui::dispatch(&mut app, "dialog-field", "color", "#010203").unwrap();
        assert!(openrigcue_ui::ui::dialogs::render(&app).contains("#010203"), "the picked colour shows");
        let error = openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "").unwrap_err();
        assert!(!error.is_empty());
        assert!(app.ui.dialog.is_none());
        assert_eq!(h.context.state.config.read().settings().native_snapshot().get("LED"), Some(&LegacyValue::None));
        drop(app.take_threads());
    }

    #[test]
    fn native_yaw_center_request_sets_metadata_without_api_runtime_or_second_status_read() {
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        assert_eq!(h.context.state.motion.read().link.status, openrigcue_devices::motion::LinkStatus::NotConnected);
        backend.request_yaw_center().unwrap();
        assert!(h.context.motion.driver_settings().yaw_shared.get().homing_requested);
        let Runtime::Own(runtime) = &backend.runtime else { unreachable!() };
        assert!(runtime.get().is_none());
    }

    #[test]
    fn cached_profile_active_context_waits_for_the_loaded_tune_without_api_runtime() {
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let initial = backend.active_profile_game().unwrap();
        h.context.telemetry.publish_game_value(&h.context.state, Some("ac".into()));
        assert_eq!(backend.game_value().unwrap(), LegacyValue::from("ac"));
        assert_eq!(backend.active_profile_game().unwrap(), initial);
        h.context.main_loop.lock().unwrap().load_current_profile(&h.context).unwrap();
        assert_eq!(backend.active_profile_game().unwrap(), "ac");
        let initial_vehicle = backend.active_profile_vehicle().unwrap();
        h.context.telemetry.publish_vehicle_value(
            &h.context.state,
            LegacyValue::from("new car"),
            Some("new car".into()),
        );
        assert_eq!(backend.session_value("vehicle_name").unwrap(), LegacyValue::from("new car"));
        assert_eq!(backend.active_profile_vehicle().unwrap(), initial_vehicle);
        h.context.main_loop.lock().unwrap().load_current_profile(&h.context).unwrap();
        assert_eq!(backend.active_profile_vehicle().unwrap(), LegacyValue::from("new car"));
        assert_eq!(h.context.state.profile.read().level, openrigcue_config::profiles::ProfileLevel::Game);
        let Runtime::Own(runtime) = &backend.runtime else { unreachable!() };
        assert!(runtime.get().is_none());
    }

    #[test]
    fn cached_category_apply_stages_only_belt_then_save_and_cancel_use_normal_storage() {
        use openrigcue_config::synced_profiles::Category;
        let h = super::super::tests::harness();
        // Normal storage: the SQLite table file, whose rows are decoded copies of what was committed.
        let dir = openrigcue_config::settings::paths::ConfigDir::at(h._tmp.path().join("cached-category"));
        openrigcue_config::settings::Configuration::first_run().save(&dir).unwrap();
        *h.context.state.config.write() = crate::store::ConfigStore::load(dir).unwrap();
        h.context.telemetry.publish_game_value(&h.context.state, Some("ac".into()));
        h.context.main_loop.lock().unwrap().load_current_profile(&h.context).unwrap();
        let before = h.context.state.config.read().profile_row("ac").unwrap().unwrap();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::DownloadedProfiles);
        let mut incoming = detached(&h.context.state.profile.read().native_tune());
        incoming.as_dict_mut().unwrap().insert("belt_surge", 17.into());
        openrigcue_ui::ui::screens::library::apply_to_current(&mut app, Category::Belt, &incoming).unwrap();
        assert_eq!(
            h.context.state.profile.read().native_tune().item_owned("belt_surge").unwrap().py_float(),
            Some(17.)
        );
        assert!(
            openrigcue_model::python_compare::native_equal(
                &h.context.state.config.read().profile_row("ac").unwrap().unwrap(),
                &before
            ),
            "staging does not write the stored row"
        );
        backend.save_profile(None).unwrap();
        let saved = h.context.state.config.read().profile_row("ac").unwrap().unwrap();
        assert_eq!(saved.item_owned("belt_surge").unwrap().py_float(), Some(17.));
        assert!(openrigcue_model::python_compare::native_equal(
            &saved.item_owned("shaker_effects").unwrap(),
            &before.item_owned("shaker_effects").unwrap()
        ));
        incoming.as_dict_mut().unwrap().insert("belt_surge", 3.into());
        openrigcue_ui::ui::screens::library::apply_to_current(&mut app, Category::Belt, &incoming).unwrap();
        backend.call(Method::Post, "profile/cancel", Some(serde_json::json!({}))).unwrap();
        h.context.main_loop.lock().unwrap().load_current_profile(&h.context).unwrap();
        assert_eq!(
            h.context.state.profile.read().native_tune().item_owned("belt_surge").unwrap().py_float(),
            Some(17.)
        );
    }

    #[test]
    fn hardware_yaw_snapshot_centered_change_reruns_the_status_rules() {
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Home);
        app.main_loop_pass();
        app.main_loop_pass();
        assert_eq!(app.ml.fatal_error, None);
        assert!(app.ml.hardware_states.contains_key("Hud"), "the status rules record every device");
        let before = backend.hardware_parameter("Yaw VR").unwrap();
        let stale = ("stale".to_owned(), "stale tooltip".to_owned());
        app.ml.hardware_states.insert("Hud".into(), stale.clone());
        app.main_loop_pass();
        assert_eq!(app.ml.hardware_states["Hud"], stale, "an unchanged snapshot does not re-run the rules");
        h.context.motion.driver_settings().yaw_shared.update(|shared| shared.centered = true);
        app.main_loop_pass();
        assert_eq!(app.ml.fatal_error, None);
        assert_ne!(app.ml.hardware_states["Hud"], stale);
        let after = backend.hardware_parameter("Yaw VR").unwrap();
        assert_eq!(before.item_owned("centered").unwrap(), LegacyValue::Bool(false));
        assert_eq!(after.item_owned("centered").unwrap(), LegacyValue::Bool(true));
        drop(app.take_threads());
    }

    #[test]
    fn hardware_raw_port_concat_error_stops_the_loop_and_keeps_the_recorded_states() {
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend, openrigcue_ui::ui::Screen::Home);
        app.main_loop_pass();
        assert_eq!(app.ml.fatal_error, None);
        let prior = ("prior".to_owned(), "prior tooltip".to_owned());
        app.ml.hardware_states.insert("Intellibox".into(), prior.clone());
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("INTELLIBOX").unwrap().as_dict_mut().unwrap().insert("com_port", 17.into());
            })
        });
        app.main_loop_pass();
        assert_eq!(app.ml.fatal_error.as_deref(), Some("TypeError: can only concatenate str (not \"int\") to str"));
        assert_eq!(app.ml.hardware_states["Intellibox"], prior, "a failed pass records no state");
        drop(app.take_threads());
    }

    #[test]
    fn hardware_auto_search_invalid_raw_value_stops_the_status_rules() {
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend, openrigcue_ui::ui::Screen::Home);
        app.main_loop_pass();
        assert_eq!(app.ml.fatal_error, None);
        let prior = ("prior".to_owned(), "prior tooltip".to_owned());
        app.ml.hardware_states.insert("Intellibox".into(), prior.clone());
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("INTELLIBOX").unwrap().as_dict_mut().unwrap().insert("auto_search", "invalid".into());
            })
        });
        app.main_loop_pass();
        assert!(app.ml.fatal_error.as_deref().unwrap().starts_with("Exception: Boolean value expected"));
        assert_eq!(app.ml.hardware_states["Intellibox"], prior, "a failed pass records no state");
        drop(app.take_threads());
    }

    #[test]
    fn hardware_parameter_metadata_reads_keep_defaults_and_do_not_wait_for_device_io() {
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        assert_eq!(backend.hardware_parameter("belt_model").unwrap(), LegacyValue::Str("".into()));
        assert_eq!(backend.hardware_parameter("belt_version").unwrap(), LegacyValue::Int(-1));
        assert_eq!(backend.hardware_parameter("belt_sn").unwrap(), LegacyValue::Str("".into()));
        assert_eq!(backend.hardware_parameter("hud_status").unwrap(), LegacyValue::Str("Not Connected".into()));
        let mut intellibox = h.context.accessories.intellibox.lock().unwrap();
        let mut iflag = h.context.accessories.iflag.lock().unwrap();
        let belt = h.context.accessories.belt.lock().unwrap();
        intellibox.set_model(2);
        intellibox.set_version(6);
        iflag.set_firmware(Some("1.4".into()));
        let processing = h.context.main_loop.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send([
                backend.hardware_parameter("intellibox_model"),
                backend.hardware_parameter("intellibox_version"),
                backend.hardware_parameter("iflag_version"),
                backend.hardware_parameter("belt_homing"),
            ])
            .unwrap()
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop((processing, intellibox, iflag, belt));
        worker.join().unwrap();
        assert_eq!(result.unwrap().map(Result::unwrap), [2.into(), 6.into(), LegacyValue::Float(1.4), 0.into()]);
    }

    #[test]
    fn hardware_snapshot_bad_raw_section_stops_before_the_status_rules() {
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend, openrigcue_ui::ui::Screen::Home);
        app.main_loop_pass();
        assert_eq!(app.ml.fatal_error, None);
        let prior = ("prior".to_owned(), "prior tooltip".to_owned());
        app.ml.hardware_states.insert("Intellibox".into(), prior.clone());
        // A section the loop body does not read before the hardware status box's snapshot (the main loop
        // reads `SIM3D.model` for the rumble test buttons first, which would be a different error).
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.insert("VNM SIMULATION", false.into());
            })
        });
        app.main_loop_pass();
        assert_eq!(app.ml.fatal_error.as_deref(), Some("AttributeError: 'bool' object has no attribute 'get'"));
        assert_eq!(app.ml.hardware_states["Intellibox"], prior, "a failed pass records no state");
        drop(app.take_threads());
    }

    #[test]
    fn hardware_snapshot_detects_raw_config_change_with_unchanged_typed_projection() {
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("INTELLIBOX").unwrap().as_dict_mut().unwrap().insert("auto_search", "True".into());
            })
        });
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Home);
        app.main_loop_pass();
        app.main_loop_pass();
        assert_eq!(app.ml.fatal_error, None);
        // Establish the raw string after the start-up passes.
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("INTELLIBOX").unwrap().as_dict_mut().unwrap().insert("auto_search", "True".into());
            });
        });
        app.main_loop_pass();
        assert_eq!(backend.native_setting("INTELLIBOX", "auto_search").unwrap(), LegacyValue::Str("True".into()));
        let stale = ("stale".to_owned(), "stale tooltip".to_owned());
        app.ml.hardware_states.insert("Intellibox".into(), stale.clone());
        app.main_loop_pass();
        assert_eq!(app.ml.hardware_states["Intellibox"], stale, "an unchanged snapshot does not re-run the rules");
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("INTELLIBOX").unwrap().as_dict_mut().unwrap().insert("auto_search", true.into());
            })
        });
        assert_eq!(backend.native_setting("INTELLIBOX", "auto_search").unwrap(), LegacyValue::Bool(true));
        app.main_loop_pass();
        assert_eq!(app.ml.fatal_error, None);
        assert_ne!(app.ml.hardware_states["Intellibox"], stale, "the raw change re-runs the status rules");
        drop(app.take_threads());
    }

    /// f2: the interface action `act` as the browser sends it.
    fn f2_act(app: &mut openrigcue_ui::app::MyApp, act: &str, arg: &str, value: &str) -> Result<(), String> {
        openrigcue_ui::ui::dispatch(app, act, arg, value)
    }

    /// f2: the opening tag of the element with `id` in `html`.
    fn f2_tag<'a>(html: &'a str, id: &str) -> &'a str {
        let start = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("no element {id}"));
        let open = html[..start].rfind('<').unwrap();
        let end = start + html[start..].find('>').unwrap();
        &html[open..=end]
    }

    /// f2: a stored `MOTION_COMPENSATION` value.
    fn f2_mc(h: &super::super::tests::Harness, path: &[&str]) -> LegacyValue {
        let raw = h.context.state.config.read().settings().native_snapshot();
        let section = raw.get("MOTION_COMPENSATION").unwrap().clone();
        path.iter().fold(section, |value, key| value.item_owned(key).unwrap())
    }

    /// f2: the title and message of the last notification.
    fn f2_last_notice(app: &openrigcue_ui::app::MyApp) -> (String, String) {
        app.ui.notices.history.last().map(|notice| (notice.title.clone(), notice.message.clone())).unwrap_or_default()
    }

    #[test]
    fn native_iflag_firmware_parameter_keeps_float_zero_and_nonfinite_values() {
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        assert_eq!(backend.accessory_status("iflag_firmware").unwrap(), LegacyValue::Int(0));
        let mut device = h.context.accessories.iflag.lock().unwrap();
        device.set_firmware(Some("1.3".into()));
        assert_eq!(backend.accessory_status("iflag_firmware").unwrap(), LegacyValue::Float(1.3));
        device.set_firmware(Some("invalid".into()));
        assert_eq!(backend.accessory_status("iflag_firmware").unwrap(), LegacyValue::Int(0));
        device.set_firmware(Some("nan".into()));
        assert!(
            matches!(backend.accessory_status("iflag_firmware").unwrap(), LegacyValue::Float(value) if value.is_nan())
        );
    }

    #[test]
    fn hardware_snapshot_state_reads_live_showing_and_dismissal_flags() {
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        assert_eq!(backend.hardware_notification_state(), HardwareNotificationState::default());
        backend.hardware_notification(HardwareNotification::Belt { needs_calibration: true }).unwrap();
        backend.hardware_notification(HardwareNotification::IflagFirmware).unwrap();
        backend.hardware_notification(HardwareNotification::YawVr { needs_center: true }).unwrap();
        assert_eq!(
            backend.hardware_notification_state(),
            HardwareNotificationState { showing_belt: true, showing_iflag: true, ..Default::default() }
        );
        let ids = h.context.state.notifications.read().items.iter().map(|item| item.id).collect::<Vec<_>>();
        for id in ids {
            h.context.state.notifications.write().dismiss(id, "none");
        }
        backend.hardware_notification(HardwareNotification::Belt { needs_calibration: false }).unwrap();
        assert_eq!(
            backend.hardware_notification_state(),
            HardwareNotificationState {
                showing_belt: false,
                showing_iflag: true,
                dismissed_yaw_vr: true,
                dismissed_belt: true,
                dismissed_iflag: true,
            }
        );
    }

    #[test]
    fn hardware_notifications_publish_at_widget_branches_and_tail_does_not_recompute_them() {
        use crate::notifications::NotificationKind;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        backend.hardware_notification(HardwareNotification::YawVr { needs_center: true }).unwrap();
        backend.hardware_notification(HardwareNotification::IflagFirmware).unwrap();
        backend.hardware_notification(HardwareNotification::Belt { needs_calibration: true }).unwrap();
        let kinds =
            || h.context.state.notifications.read().items.iter().map(|item| item.kind.clone()).collect::<Vec<_>>();
        assert_eq!(
            kinds(),
            [NotificationKind::YawVrNotCentered, NotificationKind::IflagFirmware, NotificationKind::BeltCalibration]
        );
        // An unrelated later model lookup must not be invented at the notification tail.
        h.context
            .state
            .config
            .write()
            .update(|configuration| configuration.settings.motion.model = "Missing model".into());
        backend.finish_main_loop().unwrap();
        assert_eq!(kinds().len(), 3);
        backend.hardware_notification(HardwareNotification::YawVr { needs_center: false }).unwrap();
        backend.hardware_notification(HardwareNotification::Belt { needs_calibration: false }).unwrap();
        assert_eq!(kinds(), [NotificationKind::IflagFirmware]);
        let id = h.context.state.notifications.read().items[0].id;
        h.context.state.notifications.write().dismiss(id, "none");
        backend.hardware_notification(HardwareNotification::IflagFirmware).unwrap();
        assert!(kinds().is_empty());
    }

    #[test]
    fn legacy_registry_upgrade_precedes_a_pending_factory_reset() {
        use openrigcue_config::import::{migrate::Platform, registry, sqlitedict};
        use openrigcue_config::settings::{paths::ConfigDir, Configuration};
        let temporary = tempfile::tempdir().unwrap();
        let dir = ConfigDir::at(temporary.path());
        Configuration::first_run().save(&dir).unwrap();
        std::fs::remove_file(dir.registry_file()).unwrap();
        for (key, value) in [("MotionModel", "H3"), ("Soundcard", "Remembered USB"), ("StartWithWindows", "0")] {
            sqlitedict::set_item(&dir.metadata_file(), "registry", key, &value.into()).unwrap();
        }
        rusqlite::Connection::open(dir.metadata_file())
            .unwrap()
            .execute("INSERT INTO registry (key,value) VALUES ('opaque',?)", [vec![0xff_u8]])
            .unwrap();
        std::fs::write(dir.root().join("reset.txt"), "factory_settings_trigger").unwrap();
        let restored = crate::store::ConfigStore::open(dir.clone(), Platform::Windows, None).unwrap();
        assert_eq!(restored.settings().motion.model, "H3");
        assert_eq!(restored.settings().shakers.sound_card, "Remembered USB");
        // Fresh template migrations still reset these flags.
        assert_eq!(registry::read_item(dir.root(), "StartWithWindows").unwrap(), Some("1".into()));
        let opaque: Vec<u8> = rusqlite::Connection::open(dir.registry_file())
            .unwrap()
            .query_row("SELECT value FROM registry WHERE key='opaque'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(opaque, [0xff]);
    }

    #[test]
    fn factory_reset_retains_saved_hardware_preferences_for_the_next_startup() {
        use openrigcue_config::import::migrate::Platform;
        use openrigcue_config::settings::{paths::ConfigDir, Configuration};
        let h = super::super::tests::harness();
        let dir = ConfigDir::at(h._tmp.path().join("registry-lifetime"));
        Configuration::first_run().save(&dir).unwrap();
        *h.context.state.config.write() = crate::store::ConfigStore::load(dir.clone()).unwrap();
        h.context.state.config.write().update(|configuration| {
            configuration.settings.motion.model = "H3".into();
            configuration.settings.shakers.sound_card = "Remembered USB".into();
        });
        h.context.state.session.write().game = Some("ac".into());
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        backend.save_profile(None).unwrap();
        backend.call(Method::Post, "factory-reset", None).unwrap();
        assert!(!dir.settings_file().exists());
        assert!(!dir.metadata_file().exists());
        let restored = crate::store::ConfigStore::open(dir, Platform::Windows, None).unwrap();
        assert_eq!(restored.settings().motion.model, "H3");
        assert_eq!(restored.settings().shakers.sound_card, "Remembered USB");
    }

    #[test]
    fn failed_hardware_preference_writes_do_not_abort_the_profile_commit() {
        use openrigcue_config::import::sqlitedict;
        use openrigcue_config::settings::{paths::ConfigDir, Configuration};
        let h = super::super::tests::harness();
        let dir = ConfigDir::at(h._tmp.path().join("denied-preferences"));
        Configuration::first_run().save(&dir).unwrap();
        *h.context.state.config.write() = crate::store::ConfigStore::load(dir.clone()).unwrap();
        rusqlite::Connection::open(dir.registry_file())
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER deny_hardware_hints BEFORE INSERT ON registry
             WHEN NEW.key IN ('MotionModel','Soundcard')
             BEGIN SELECT RAISE(ABORT, 'preference write denied'); END;",
            )
            .unwrap();
        let mut native = detached(&h.context.state.profile.read().native_tune());
        native.as_dict_mut().unwrap().insert("marker", 55.into());
        h.context.state.profile.write().replace_native_tune(native);
        h.context.state.session.write().game = Some("ac".into());
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        backend.save_profile(None).unwrap();
        let row = sqlitedict::read_item(&dir.profiles_file(), "unnamed", "ac").unwrap().unwrap();
        assert_eq!(row.item_owned("marker").unwrap(), 55.into());
        assert!(sqlitedict::read_item(&dir.registry_file(), "registry", "MotionModel").unwrap().is_none());
        backend.store_shaker_effects().unwrap();
    }

    #[test]
    fn save_records_hardware_preferences_before_profile_errors_without_committing_pending_rows() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        use openrigcue_config::settings::{paths::ConfigDir, Configuration};
        let h = super::super::tests::harness();
        let dir = ConfigDir::at(h._tmp.path().join("save-preferences"));
        Configuration::first_run().save(&dir).unwrap();
        *h.context.state.config.write() = crate::store::ConfigStore::load(dir.clone()).unwrap();
        sqlitedict::set_item(&dir.registry_file(), "registry", "StartWithWindows", &LegacyValue::from("0")).unwrap();
        std::fs::remove_file(dir.settings_file()).unwrap();
        rusqlite::Connection::open(dir.registry_file())
            .unwrap()
            .execute("INSERT INTO registry (key,value) VALUES ('opaque',?)", [vec![0xff_u8]])
            .unwrap();
        {
            let mut store = h.context.state.config.write();
            store.update(|configuration| {
                configuration.settings.edit_native(|raw| {
                    raw.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap().insert("model", "H3".into());
                    raw.get_mut("SHAKEBOX")
                        .unwrap()
                        .as_dict_mut()
                        .unwrap()
                        .insert("soundcard", "Remembered USB".into());
                })
            });
            let mut pending = store.profile_row("ac").unwrap().unwrap();
            pending.as_dict_mut().unwrap().insert("pending_marker", 999.into());
            store.stage_game_row("ac", Some(pending)).unwrap();
        }
        h.context.state.session.write().game = Some("missing-profile".into());
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        assert!(backend.save_profile(None).is_err());
        assert_eq!(sqlitedict::read_item(&dir.registry_file(), "registry", "MotionModel").unwrap(), Some("H3".into()));
        assert_eq!(
            sqlitedict::read_item(&dir.registry_file(), "registry", "Soundcard").unwrap(),
            Some("Remembered USB".into())
        );
        assert_eq!(
            sqlitedict::read_item(&dir.registry_file(), "registry", "StartWithWindows").unwrap(),
            Some("0".into())
        );
        assert!(!dir.settings_file().exists(), "the hint setter must not recreate a removed session INI");
        let disk = sqlitedict::read_item(&dir.profiles_file(), "unnamed", "ac").unwrap().unwrap();
        assert!(disk.item_owned("pending_marker").is_err());
        assert_eq!(
            h.context.state.config.read().profile_row("ac").unwrap().unwrap().item_owned("pending_marker").unwrap(),
            999.into()
        );

        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap().insert("model", "H2".into());
                raw.get_mut("SHAKEBOX").unwrap().as_dict_mut().unwrap().remove("soundcard");
            })
        });
        let error = backend.save_profile(None).unwrap_err();
        assert!(error.detail.contains("KeyError") && error.detail.contains("soundcard"), "{error}");
        assert_eq!(sqlitedict::read_item(&dir.registry_file(), "registry", "MotionModel").unwrap(), Some("H2".into()));
        assert_eq!(
            sqlitedict::read_item(&dir.registry_file(), "registry", "Soundcard").unwrap(),
            Some("Remembered USB".into())
        );

        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap().insert("model", LegacyValue::Int(7));
            })
        });
        let error = backend.save_profile(None).unwrap_err();
        assert!(error.detail.contains("TypeError"), "{error}");
        assert_eq!(sqlitedict::read_item(&dir.registry_file(), "registry", "MotionModel").unwrap(), Some("H2".into()));
    }

    #[test]
    fn native_save_uses_current_game_and_displayed_vehicle_without_the_main_loop() {
        use openrigcue_config::import::value::LegacyValue;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        h.context.state.session.write().game = Some("ac".into());
        h.context.main_loop.lock().unwrap().synchronize_profile(&h.context).unwrap();
        let mut native = detached(&h.context.state.profile.read().native_tune());
        native.as_dict_mut().unwrap().insert("marker", 42.into());
        h.context.state.profile.write().replace_native_tune(native);
        {
            let mut session = h.context.state.session.write();
            session.game = Some("acc".into());
            session.set_vehicle_value(LegacyValue::Str("New SDK car".into()), Some("New SDK car".into()));
        }
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let guard = h.context.main_loop.lock().unwrap();
        let completed_while_busy = std::thread::scope(|scope| {
            let (send, receive) = mpsc::sync_channel(1);
            let backend = &backend;
            scope.spawn(move || send.send(backend.save_profile(Some("Displayed car"))).unwrap());
            let early = receive.recv_timeout(Duration::from_secs(2));
            drop(guard);
            match early {
                Ok(result) => {
                    result.unwrap();
                    true
                }
                Err(_) => {
                    receive.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                    false
                }
            }
        });
        assert!(completed_while_busy, "saving the source's explicit arguments does not query the main-loop tracker");
        {
            let store = h.context.state.config.read();
            let row = store.profile_row("acc").unwrap().unwrap();
            let vehicles = row.item_owned("vehicles").unwrap().resolved();
            let vehicles = vehicles.as_dict().unwrap();
            assert_eq!(vehicles.get("Displayed car").unwrap().item_owned("marker").unwrap(), LegacyValue::Int(42));
            assert!(!vehicles.contains("New SDK car"));
            let old = store.profile_row("ac").unwrap().unwrap();
            assert!(!old.item_owned("vehicles").unwrap().resolved().as_dict().unwrap().contains("Displayed car"));
        }
        let mut native = detached(&h.context.state.profile.read().native_tune());
        native.as_dict_mut().unwrap().insert("marker", 43.into());
        h.context.state.profile.write().replace_native_tune(native);
        backend.save_profile(Some("")).unwrap();
        let row = h.context.state.config.read().profile_row("acc").unwrap().unwrap();
        assert_eq!(row.item_owned("marker").unwrap(), LegacyValue::Int(43));
        assert!(row.item_owned("vehicles").unwrap().resolved().as_dict().unwrap().contains("Displayed car"));
    }

    #[test]
    fn manual_detection_publishes_assignments_before_waiting_for_the_previous_vehicle() {
        use openrigcue_config::import::value::LegacyValue;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        let h = super::super::tests::harness();
        h.context.state.session.write().game = Some("ac".into());
        h.context.telemetry.publish_vehicle_value(&h.context.state, "Previous car".into(), Some("Previous car".into()));
        {
            let mut processing = h.context.main_loop.lock().unwrap();
            processing.synchronize_profile(&h.context).unwrap();
            processing.remember_actual_vehicle(&h.context);
            processing.remember_profile_inputs(&h.context);
        }
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let guard = h.context.main_loop.lock().unwrap();
        let before_tracker_reset = std::thread::scope(|scope| {
            let (send, receive) = mpsc::sync_channel(1);
            scope.spawn(move || send.send(backend.put("games/detection", serde_json::json!({"entry":"acc"}))).unwrap());
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                if h.context.state.controls.read().forced_game.as_deref() == Some("acc")
                    && h.context.state.session.read().vehicle_value() == LegacyValue::Int(0)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let assignments =
                (h.context.state.controls.read().forced_game.clone(), h.context.state.session.read().vehicle_value());
            drop(guard);
            receive.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assignments
        });
        assert_eq!(before_tracker_reset, (Some("acc".into()), LegacyValue::Int(0)));
        assert_eq!(h.context.telemetry.lock().vehicle_value(), &LegacyValue::Int(0));
        assert_eq!(
            h.context.main_loop.lock().unwrap().displayed_native_vehicle(&LegacyValue::Int(0)),
            openrigcue_config::profiles::Vehicle::Unknown
        );
        assert_eq!(
            h.context.state.session.read().game.as_deref(),
            Some("ac"),
            "game detection still owns game_running"
        );
    }

    #[test]
    fn cancelling_profile_edit_ends_the_edit_and_the_next_pass_shows_the_session() {
        use openrigcue_ui::ui::session::State;
        for (started_with_listener, listener_at_cancel, expected) in [
            (false, false, State::Standby),
            (false, true, State::Telemetry),
            (true, true, State::Telemetry),
            (true, false, State::GameRunning),
        ] {
            let h = super::super::tests::harness();
            h.context.state.session.write().listener = started_with_listener.then(|| "ac".into());
            let backend = harness_backend(&h);
            backend
                .call(Method::Post, "profile/edit", Some(serde_json::json!({"game":"ac","vehicle":"Editing car"})))
                .unwrap();
            assert_eq!(backend.control_value(&["profile_edit"]).unwrap(), LegacyValue::Bool(true));
            h.context.state.session.write().game = Some("ac".into());
            {
                let mut processing = h.context.main_loop.lock().unwrap();
                processing.synchronize_profile(&h.context).unwrap();
                processing.remember_actual_vehicle(&h.context);
                processing.remember_profile_inputs(&h.context);
            }
            let generation = h.context.state.profile.read().load_generation;
            let mut app = mounted_app(backend, openrigcue_ui::ui::Screen::Home);
            assert_eq!(
                app.ui.session.vehicle,
                "Editing car",
                "started_with_listener={started_with_listener}, session={:?}, previous={:?}",
                h.context.state.session.snapshot(),
                h.context.main_loop.previous_vehicle_value()
            );
            app.ml.previous_location = Some("Previous Track".into());
            h.context.state.session.write().listener = listener_at_cancel.then(|| "ac".into());

            // Revert while a profile is edited ends the edit at once.
            f2_act(&mut app, "revert", "", "").unwrap();
            assert!(app.ui.dialog.is_none(), "no confirmation while editing");
            assert_eq!(f2_last_notice(&app).0, "Changes reverted");
            assert_eq!(
                app.ml.previous_location,
                if expected == State::Standby { None } else { Some("Previous Track".into()) }
            );
            assert!(h.context.state.controls.read().profile_edit.is_none());
            assert!(h.context.state.controls.read().forced_game.is_none());
            assert_eq!(h.context.state.profile.read().load_generation, generation, "clearing does not load a profile");

            openrigcue_ui::ui::tick(&mut app);
            assert_eq!(app.ui.session.state, expected, "listener_at_cancel={listener_at_cancel}");
            assert_eq!(app.ui.session.vehicle, if expected == State::Standby { "" } else { "Editing car" });
            if expected == State::Standby {
                assert!(app.ui.session.game.is_empty());
            }
            let label = openrigcue_ui::ui::shell::profile_label(&app);
            assert!(!label.contains("Editing Profile"), "{label}");
            drop(app.take_threads());
        }
    }

    #[test]
    fn import_and_cancel_request_reload_without_waiting_for_the_main_loop() {
        use openrigcue_config::catalog::Catalog;
        use openrigcue_config::profiles::{export_profile, read_profile_file, ExportMetadata, Vehicle};
        use openrigcue_config::settings::Configuration;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        h.context.state.session.write().game = Some("ac".into());
        h.context.main_loop.lock().unwrap().synchronize_profile(&h.context).unwrap();
        let generation = h.context.state.profile.read().load_generation;
        let mut profiles = Configuration::first_run().profiles;
        let mut row = profiles.native_table().get("ac").unwrap().clone();
        row.as_dict_mut().unwrap().insert("marker", 42.into());
        profiles.set_native_game("ac", row);
        let meta = ExportMetadata { motion_model: "Fixture", shaker_layout: "Fixture", builtin_layouts: &[] };
        let (_, bytes) = export_profile(&profiles, Catalog::builtin(), "ac", &Vehicle::Unknown, None, &meta).unwrap();
        let file = read_profile_file(&bytes).unwrap();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let guard = h.context.main_loop.lock().unwrap();
        let completed_while_busy = std::thread::scope(|scope| {
            let (send, receive) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let result = backend
                    .confirm_profile_import(&file)
                    .and_then(|()| backend.call(Method::Post, "profile/cancel", None).map(|_| ()));
                send.send(result).unwrap();
            });
            let early = receive.recv_timeout(Duration::from_secs(15));
            assert_eq!(h.context.state.profile.read().load_generation, generation);
            drop(guard);
            match early {
                Ok(result) => {
                    result.unwrap();
                    true
                }
                Err(_) => {
                    receive.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                    false
                }
            }
        });
        assert!(completed_while_busy, "the source flag assignment does not wait for the main loop");
        {
            let mut main_loop = h.context.main_loop.lock().unwrap();
            main_loop.synchronize_profile(&h.context).unwrap();
            let profile = h.context.state.profile.read();
            assert!(profile.load_generation > generation);
            assert_eq!(profile.native_tune().item_owned("marker").ok(), Some(42.into()));
            let loaded = profile.load_generation;
            drop(profile);
            main_loop.remember_actual_vehicle(&h.context);
            main_loop.remember_profile_inputs(&h.context);
            main_loop.synchronize_profile(&h.context).unwrap();
            assert_eq!(h.context.state.profile.read().load_generation, loaded, "coalesced requests are consumed once");
        }
    }

    #[test]
    fn profile_import_confirms_offered_content_after_file_or_session_changes() {
        use openrigcue_config::catalog::Catalog;
        use openrigcue_config::import::value::LegacyValue;
        use openrigcue_config::profiles::{export_profile, ExportMetadata, Vehicle};
        use openrigcue_config::settings::Configuration;
        use openrigcue_ui::ui::dialogs::Dialog;
        use openrigcue_ui::ui::profiles::offer_launcher_import;
        for change in ["deleted", "replaced", "session", "rejected", "metadata"] {
            let h = super::super::tests::harness();
            let mut original = Configuration::first_run().profiles;
            let mut row = original.native_table().get("ac").unwrap().clone();
            row.as_dict_mut().unwrap().insert("marker", 42.into());
            original.set_native_game("ac", row);
            let meta = ExportMetadata { motion_model: "Fixture", shaker_layout: "Fixture", builtin_layouts: &[] };
            let (_, bytes) =
                export_profile(&original, Catalog::builtin(), "ac", &Vehicle::Unknown, None, &meta).unwrap();
            let path = h._tmp.path().join("offered.srs");
            std::fs::write(&path, &bytes).unwrap();
            let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Profiles);
            app.docs.status = serde_json::json!({"arguments":{"profile_path":path}});
            // The launcher hands the file over: it is read and checked, then the import question opens.
            offer_launcher_import(&mut app, "new_instance").unwrap();
            assert!(matches!(app.ui.dialog, Some(Dialog::Import { .. })), "{change}");
            let marker = || {
                h.context
                    .state
                    .config
                    .read()
                    .profile_row("ac")
                    .unwrap()
                    .unwrap()
                    .as_dict()
                    .unwrap()
                    .get("marker")
                    .cloned()
            };
            match change {
                "deleted" => std::fs::remove_file(&path).unwrap(),
                "replaced" => {
                    // A file that is no profile is refused and explained.
                    std::fs::write(&path, b"replacement is not a profile").unwrap();
                    offer_launcher_import(&mut app, "new_instance").unwrap();
                    assert!(matches!(app.ui.dialog, Some(Dialog::Message { .. })), "{change}");
                    drop(app.take_threads());
                    continue;
                }
                "metadata" => {
                    use std::io::Write;
                    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
                    archive.start_file("invalid.srs", zip::write::SimpleFileOptions::default()).unwrap();
                    archive.write_all(b"{}").unwrap();
                    std::fs::write(&path, archive.finish().unwrap().into_inner()).unwrap();
                    offer_launcher_import(&mut app, "new_instance").unwrap();
                    assert!(!matches!(app.ui.dialog, Some(Dialog::Import { .. })), "invalid content is refused");
                    f2_act(&mut app, "dialog-ok", "", "").unwrap();
                    assert_eq!(marker(), None, "invalid parsed content must replace the older offer");
                    drop(app.take_threads());
                    continue;
                }
                "session" | "rejected" => {
                    let mut session = h.context.state.session.write();
                    session.game = Some("acc".into());
                    session.set_vehicle_value(LegacyValue::Str("Other car".into()), Some("Other car".into()));
                }
                _ => unreachable!(),
            }
            if change == "rejected" {
                // A profile the service refuses for the session in use is explained.
                let mut row = original.native_table().get("ac").unwrap().clone();
                row.as_dict_mut().unwrap().insert("marker", 77.into());
                original.set_native_game("ac", row);
                let (_, bytes) =
                    export_profile(&original, Catalog::builtin(), "ac", &Vehicle::Unknown, None, &meta).unwrap();
                std::fs::write(&path, &bytes).unwrap();
                offer_launcher_import(&mut app, "new_instance").unwrap();
                let Some(Dialog::Message { text, .. }) = &app.ui.dialog else { panic!("{:?}", app.ui.dialog) };
                assert!(text.contains("does not correspond to the profile now in use"), "{text}");
                drop(app.take_threads());
                continue;
            }
            f2_act(&mut app, "dialog-ok", "", "").unwrap();
            assert!(app.ui.dialog.is_none());
            assert_eq!(marker(), Some(LegacyValue::Int(42)), "{change}");
            assert_eq!(f2_last_notice(&app).0, "Import Profile");
            if change == "session" {
                assert!(h.context.state.controls.read().profile_edit.is_none());
                assert_eq!(h.context.state.session.read().game.as_deref(), Some("acc"));
            } else {
                assert_eq!(h.context.state.controls.read().profile_edit.as_ref().unwrap().game, "ac");
            }
            // The confirmation consumes the offer; another confirmation cannot reimport it over newer edits.
            let mut newer = h.context.state.config.read().profile_row("ac").unwrap().unwrap();
            newer.as_dict_mut().unwrap().insert("marker", 97.into());
            h.context.state.config.write().stage_game_row("ac", Some(newer)).unwrap();
            f2_act(&mut app, "dialog-ok", "", "").unwrap();
            openrigcue_ui::ui::profiles::import_confirmed(&mut app);
            assert_eq!(marker(), Some(LegacyValue::Int(97)), "{change}");
            drop(app.take_threads());
        }
    }

    #[test]
    fn null_import_key_survives_save_and_reload_without_becoming_text_none() {
        use openrigcue_config::import::{
            sqlitedict,
            value::{Key, LegacyValue},
        };
        use openrigcue_config::profiles::{read_profile_file, ProfileLevel, ProfileStore};
        use openrigcue_config::settings::{paths::ConfigDir, Configuration};
        use std::io::Write;
        let h = super::super::tests::harness();
        let dir = ConfigDir::at(h._tmp.path().join("null-key-profile"));
        Configuration::first_run().save(&dir).unwrap();
        *h.context.state.config.write() = crate::store::ConfigStore::load(dir.clone()).unwrap();
        let base = h.context.state.config.read().profile_row("ac").unwrap().unwrap();
        let base_gear = base.item_owned("gear_shift_perc").unwrap().clone();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        for (vehicle, gear) in [(serde_json::Value::Null, 43), (serde_json::json!("None"), 61)] {
            let mut value = openrigcue_config::tunes::FactoryTunes::builtin().get("ac").unwrap().to_json();
            let fields = value.as_object_mut().unwrap();
            fields.insert("game".into(), serde_json::json!("ac"));
            fields.insert("vehicle".into(), vehicle);
            fields.insert("srs_version".into(), serde_json::json!("fixture"));
            fields.insert("gear_shift_perc".into(), serde_json::json!(gear));
            let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            archive.start_file("fixture.srs", zip::write::SimpleFileOptions::default()).unwrap();
            archive.write_all(value.to_string().as_bytes()).unwrap();
            let file = read_profile_file(&archive.finish().unwrap().into_inner()).unwrap();
            backend.confirm_profile_import(&file).unwrap();
        }
        let pending = h.context.state.config.read().profile_row("ac").unwrap().unwrap();
        let vehicles = pending.item_owned("vehicles").unwrap().resolved();
        let vehicles = vehicles.as_dict().unwrap();
        assert_eq!(vehicles.get_key(&Key::None).unwrap().item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(43));
        assert_eq!(vehicles.get("None").unwrap().item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(61));
        assert_eq!(vehicles.len(), 2);
        let disk = sqlitedict::read_item(&dir.profiles_file(), "unnamed", "ac").unwrap().unwrap();
        assert!(disk.item_owned("vehicles").unwrap().as_dict().unwrap().get_key(&Key::None).is_none());
        let (selected, level) = ProfileStore::resolve_native_identity_row("ac", &LegacyValue::None, &pending).unwrap();
        assert_eq!(level, ProfileLevel::Game, "None retains the original no-vehicle sentinel rule");
        assert_eq!(selected.item_owned("gear_shift_perc").unwrap(), base_gear);
        let (selected, level) = ProfileStore::resolve_native_identity_row("ac", &"None".into(), &pending).unwrap();
        assert_eq!(level, ProfileLevel::Vehicle);
        assert_eq!(selected.item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(61));
        backend.begin_profile_edit("ac", &LegacyValue::None).unwrap();
        h.context.state.session.write().game = Some("ac".into());
        h.context.main_loop.lock().unwrap().synchronize_profile(&h.context).unwrap();
        assert_eq!(h.context.state.profile.read().level, ProfileLevel::Game);
        backend.save_profile(None).unwrap();
        let reopened = Configuration::load(&dir).unwrap();
        let row = reopened.profiles.native_table().get("ac").unwrap().clone();
        let vehicles = row.item_owned("vehicles").unwrap().resolved();
        let vehicles = vehicles.as_dict().unwrap();
        assert_eq!(vehicles.get_key(&Key::None).unwrap().item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(43));
        assert_eq!(vehicles.get("None").unwrap().item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(61));
    }

    #[test]
    fn boolean_import_key_survives_native_save_and_reload_with_numeric_aliases() {
        use openrigcue_model::native_value::{Key, LegacyValue};
        scalar_import_roundtrip(
            "boolean-key-profile",
            Key::Bool(true),
            "True",
            &[(serde_json::json!(true), 43), (serde_json::json!("True"), 61), (serde_json::json!(1), 47)],
            &[LegacyValue::Bool(true), LegacyValue::Int(1), LegacyValue::Float(1.0)],
        );
    }

    #[test]
    fn float_import_keys_survive_native_save_and_reload_with_numeric_aliases() {
        use openrigcue_model::native_value::{Key, LegacyValue};
        scalar_import_roundtrip(
            "float-fractional-key",
            Key::float(1.5).unwrap(),
            "1.5",
            &[(serde_json::json!(1.5), 43), (serde_json::json!("1.5"), 61), (serde_json::json!(1.5), 47)],
            &[LegacyValue::Float(1.5)],
        );
        scalar_import_roundtrip(
            "float-integral-key",
            Key::float(1.0).unwrap(),
            "1.0",
            &[(serde_json::json!(1.0), 43), (serde_json::json!("1.0"), 61), (serde_json::json!(true), 47)],
            &[LegacyValue::Bool(true), LegacyValue::Int(1), LegacyValue::Float(1.0)],
        );
    }

    fn scalar_import_roundtrip(
        label: &str,
        stored_key: openrigcue_model::native_value::Key,
        text_name: &str,
        imports: &[(serde_json::Value, i64)],
        lookups: &[openrigcue_model::native_value::LegacyValue],
    ) {
        use openrigcue_config::import::{
            sqlitedict,
            value::{Key, LegacyValue},
        };
        use openrigcue_config::profiles::{read_profile_file, ProfileLevel, ProfileStore};
        use openrigcue_config::settings::{paths::ConfigDir, Configuration};
        use std::io::Write;
        let h = super::super::tests::harness();
        let dir = ConfigDir::at(h._tmp.path().join(label));
        Configuration::first_run().save(&dir).unwrap();
        *h.context.state.config.write() = crate::store::ConfigStore::load(dir.clone()).unwrap();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        for (vehicle, gear) in imports {
            let mut value = openrigcue_config::tunes::FactoryTunes::builtin().get("ac").unwrap().to_json();
            let fields = value.as_object_mut().unwrap();
            fields.insert("game".into(), serde_json::json!("ac"));
            fields.insert("vehicle".into(), vehicle.clone());
            fields.insert("srs_version".into(), serde_json::json!("fixture"));
            fields.insert("gear_shift_perc".into(), serde_json::json!(gear));
            let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            archive.start_file("fixture.srs", zip::write::SimpleFileOptions::default()).unwrap();
            archive.write_all(value.to_string().as_bytes()).unwrap();
            let file = read_profile_file(&archive.finish().unwrap().into_inner()).unwrap();
            backend.confirm_profile_import(&file).unwrap();
        }
        let pending = h.context.state.config.read().profile_row("ac").unwrap().unwrap();
        let vehicles = pending.item_owned("vehicles").unwrap().resolved();
        let vehicles = vehicles.as_dict().unwrap();
        assert_eq!(vehicles.keys().cloned().collect::<Vec<_>>(), [stored_key.clone(), Key::from(text_name)]);
        assert_eq!(vehicles.get_key(&stored_key).unwrap().item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(47));
        assert_eq!(vehicles.get(text_name).unwrap().item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(61));
        let disk = sqlitedict::read_item(&dir.profiles_file(), "unnamed", "ac").unwrap().unwrap();
        assert!(disk.item_owned("vehicles").unwrap().as_dict().unwrap().get_key(&stored_key).is_none());
        for vehicle in lookups {
            let (selected, level) = ProfileStore::resolve_native_identity_row("ac", vehicle, &pending).unwrap();
            assert_eq!(level, ProfileLevel::Vehicle);
            assert_eq!(selected.item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(47));
        }
        backend.begin_profile_edit("ac", &LegacyValue::Int(0)).unwrap();
        h.context.state.session.write().game = Some("ac".into());
        h.context.main_loop.lock().unwrap().synchronize_profile(&h.context).unwrap();
        backend.save_profile(None).unwrap();
        let reopened = Configuration::load(&dir).unwrap();
        let row = reopened.profiles.native_table().get("ac").unwrap().clone();
        let vehicles = row.item_owned("vehicles").unwrap().resolved();
        let vehicles = vehicles.as_dict().unwrap();
        assert_eq!(vehicles.keys().cloned().collect::<Vec<_>>(), [stored_key.clone(), Key::from(text_name)]);
        assert_eq!(vehicles.get_key(&stored_key).unwrap().item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(47));
        assert_eq!(vehicles.get(text_name).unwrap().item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(61));
    }

    #[test]
    fn game_feature_reads_published_game_without_waiting_for_the_main_loop() {
        use std::sync::mpsc;
        use std::time::Duration;
        let features = openrigcue_ui::catalog::features();
        let (column, supported, unsupported) = features
            .columns
            .iter()
            .find_map(|column| {
                let supported = features.games.iter().find(|(_, row)| row.contains(column))?.0.clone();
                let unsupported = features.games.iter().find(|(_, row)| !row.contains(column))?.0.clone();
                Some((column.clone(), supported, unsupported))
            })
            .unwrap();
        let h = super::super::tests::harness();
        h.context.telemetry.publish_game_value(&h.context.state, Some(unsupported));
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let mut app = openrigcue_ui::app::MyApp::new(backend);
        app.docs.status = serde_json::json!({"session":{"game":supported}});
        let guard = h.context.main_loop.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || send.send(app.game_feature(&column)).unwrap());
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(guard);
        worker.join().unwrap();
        assert!(result.is_ok(), "reading the current game must not wait for the MainLoop lock");
        assert_eq!(result.unwrap(), Some(false));
    }

    #[test]
    fn game_port_open_reads_native_values_without_waiting_for_the_main_loop() {
        use openrigcue_ui::ui::dialogs_hardware::{current, Dialog};
        use openrigcue_ui::ui::screens::games;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        h.context.telemetry.publish_game_value(&h.context.state, None);
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Console);
        if !app.ui.devices.games.console.loaded {
            f2_act(&mut app, "action", "Console:reload:", "").unwrap();
        }
        let index = app
            .ui
            .devices
            .games
            .console
            .games
            .iter()
            .position(|game| game.port_key.as_deref() == Some("gt7"))
            .expect("the Console list contains GT7");
        let setup = format!("Console:setup:{index}");
        h.context.state.config.write().update(|config| {
            config.settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("gt7", LegacyValue::Bytes(b"3_2_740".to_vec()));
            });
        });
        let guard = h.context.main_loop.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = f2_act(&mut app, "action", &setup, "").map(|()| match current(&app) {
                Some(Dialog::Games(games::Dialog::Setup(setup))) => Some((setup.port.clone(), setup.locked)),
                _ => None,
            });
            send.send(result).unwrap();
            (app, setup)
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(guard);
        let (mut app, setup) = worker.join().unwrap();
        assert_eq!(
            result.expect("opening the port waited for the main loop").unwrap(),
            Some(("32740".to_owned(), false))
        );
        assert!(openrigcue_ui::ui::dialogs::render(&app).contains("value=\"32740\""));
        f2_act(&mut app, "dialog-close", "", "").unwrap();
        for (native, error_class) in [(LegacyValue::None, "TypeError:"), (LegacyValue::Float(f64::NAN), "ValueError:")]
        {
            h.context.state.config.write().update(|config| {
                config.settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("gt7", native);
                });
            });
            let error = f2_act(&mut app, "action", &setup, "").unwrap_err();
            assert!(error.starts_with(error_class), "{error}");
            assert!(app.ui.dialog.is_none(), "no dialog opens");
        }
        h.context.state.config.write().update(|config| {
            config.settings.edit_native(|raw| raw.insert("NETWORK_PORT", LegacyValue::None));
        });
        assert_eq!(
            f2_act(&mut app, "action", &setup, "").unwrap_err(),
            "TypeError: 'NoneType' object is not subscriptable"
        );
        assert!(app.ui.dialog.is_none());
        drop(app.take_threads());
    }

    #[test]
    fn game_port_confirmation_keeps_native_containers_and_no_change_order() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let initial = LegacyValue::List(vec!["127.0.0.1:1234".into()]);
        backend.assign_setting_value("PORT_FORWARDING", "gt7", &initial).unwrap();
        let revision = h.context.state.config.read().revision();
        let current = h.context.state.config.read().settings().network_ports.get("gt7").unwrap();
        assert!(!backend.confirm_game_port("gt7", i64::from(current), &initial).unwrap());
        assert_eq!(h.context.state.config.read().revision(), revision);
        let tuple = LegacyValue::Tuple(vec!["127.0.0.1:5678".into()]);
        assert!(backend.confirm_game_port("gt7", i64::from(current), &tuple).unwrap());
        let raw = h.context.state.config.read().settings().native_snapshot();
        assert_eq!(raw.get("PORT_FORWARDING").unwrap().item_owned("gt7").unwrap(), tuple);
        assert_eq!(raw.get("NETWORK_PORT").unwrap().item_owned("gt7").unwrap(), LegacyValue::Int(i64::from(current)));
        let revision = h.context.state.config.read().revision();
        assert!(!backend.confirm_game_port("gt7", i64::from(current), &tuple).unwrap());
        assert_eq!(h.context.state.config.read().revision(), revision);
        for section in ["PORT_FORWARDING", "NETWORK_PORT"] {
            h.context.state.config.write().update(|configuration| {
                configuration.settings.edit_native(|raw| raw.insert(section, LegacyValue::None));
            });
            let before = h.context.state.config.read().settings().native_snapshot();
            assert!(backend.confirm_game_port("gt7", 20999, &initial).unwrap_err().detail.starts_with("TypeError:"));
            assert!(h.context.state.config.read().settings().native_snapshot() == before);
            h.context.state.config.write().update(|configuration| {
                configuration.settings.edit_native(|settings| *settings = raw.clone());
            });
        }
    }

    #[test]
    fn native_mc_parameters_preserve_readout_kinds_and_calibrate_without_device_locks() {
        use openrigcue_ui::ui::screens::compensation::STATUS_GROUP;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let values = h.context.accessories.motion_sensor_values();
        for (key, value) in [
            ("pitch", LegacyValue::Float(f64::NAN)),
            ("roll", LegacyValue::Float(f64::INFINITY)),
            ("yaw", LegacyValue::Float(-0.0)),
            ("heave", LegacyValue::Bool(true)),
            ("surge", LegacyValue::Str("bad".into())),
            ("raw_pitch", LegacyValue::Float(12.5)),
        ] {
            values.set(key, value);
        }
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Compensation);
        let loop_guard = h.context.main_loop.lock().unwrap();
        let imu_guard = h.context.accessories.imu.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            // Check again reads the live values; the pitch calibration asks, then calibrates.
            let result = f2_act(&mut app, "action", "Motion Compensation:check:", "")
                .and_then(|()| f2_act(&mut app, "action", "Motion Compensation:pitch:0", ""))
                .and_then(|()| f2_act(&mut app, "dialog-ok", "", ""));
            send.send(result).unwrap();
            app
        });
        let independent = receive.recv_timeout(Duration::from_secs(2));
        drop(imu_guard);
        drop(loop_guard);
        let mut app = worker.join().unwrap();
        independent.expect("motion parameters waited for control/device locks").unwrap();
        let shown = |app: &openrigcue_ui::app::MyApp, axis: &str| {
            app.ui.devices.compensation.sensor.iter().find(|(key, _)| key == axis).map(|(_, value)| value.clone())
        };
        for (axis, expected) in [("pitch", "nan"), ("roll", "inf"), ("yaw", "-0.0"), ("heave", "1"), ("surge", "0")] {
            assert_eq!(shown(&app, axis).as_deref(), Some(expected), "{axis}");
        }
        f2_act(&mut app, "section", "", &STATUS_GROUP.to_string()).unwrap();
        let html = openrigcue_ui::ui::screens::main(&app);
        assert!(html.contains("Yaw -0.0 · Heave 1"), "the axis values line shows the readouts");
        assert_eq!(f2_last_notice(&app).0, "Calibration complete");
        assert!(matches!(values.get("pitch"), Some(LegacyValue::Float(value)) if value.is_nan()));
        assert_eq!(f2_mc(&h, &["pitch", "offset"]), LegacyValue::Float(-12.5));
        drop(app.take_threads());
    }

    #[test]
    fn mc_initial_readouts_keep_integer_zero() {
        let h = super::super::tests::harness();
        let app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Compensation);
        let sensor = &app.ui.devices.compensation.sensor;
        for axis in ["pitch", "roll", "sway", "surge", "yaw", "heave"] {
            assert_eq!(
                sensor.iter().find(|(key, _)| key == axis).map(|(_, value)| value.as_str()),
                Some("0"),
                "{axis}"
            );
        }
        let mut app = app;
        f2_act(&mut app, "section", "", &openrigcue_ui::ui::screens::compensation::STATUS_GROUP.to_string()).unwrap();
        assert!(
            openrigcue_ui::ui::screens::main(&app).contains("Pitch 0 · Roll 0 · Yaw 0 · Heave 0 · Sway 0 · Surge 0")
        );
        drop(app.take_threads());
    }

    #[test]
    fn mc_readouts_do_not_wait_for_imu_io() {
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Home);
        let guard = h.context.accessories.imu.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            // Opening the screen reads the live values, and so does Check again.
            openrigcue_ui::ui::navigate(&mut app, openrigcue_ui::ui::Screen::Compensation, 0);
            send.send(f2_act(&mut app, "action", "Motion Compensation:check:", "")).unwrap();
            app
        });
        let independent = receive.recv_timeout(Duration::from_secs(2));
        drop(guard);
        let mut app = worker.join().unwrap();
        independent.expect("readouts waited for the IMU device lock").unwrap();
        assert_eq!(app.ui.devices.compensation.sensor.len(), 6);
        drop(app.take_threads());
    }

    #[test]
    fn mc_calibration_failure_is_returned_and_warned_without_a_write() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Compensation);
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().insert("pitch", LegacyValue::None);
            });
        });
        f2_act(&mut app, "action", "Motion Compensation:pitch:0", "").unwrap();
        assert!(openrigcue_ui::ui::dialogs::render(&app).contains("Calibrate Pitch"));
        let error = f2_act(&mut app, "dialog-ok", "", "").unwrap_err();
        assert_eq!(error, "TypeError: 'NoneType' object does not support item assignment");
        assert!(app.ui.dialog.is_none(), "the dialog closes before the calibration");
        assert_eq!(f2_last_notice(&app), ("Alert".into(), error));
        assert_eq!(f2_mc(&h, &["pitch"]), LegacyValue::None);
        drop(app.take_threads());
    }

    #[test]
    fn mc_yaw_calibration_does_not_wait_for_imu_io() {
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        h.context
            .accessories
            .motion_sensor_values()
            .set("raw_yaw", openrigcue_model::native_value::LegacyValue::Float(12.5));
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let guard = h.context.accessories.imu.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(backend.post("compensation/actions/yaw_calibrate", serde_json::json!({}))).unwrap();
        });
        let independent = receive.recv_timeout(Duration::from_secs(2));
        drop(guard);
        worker.join().unwrap();
        independent.expect("yaw calibration waited for the IMU device lock").unwrap();
    }

    #[test]
    fn mc_rows_show_the_stored_values_and_rendering_never_writes() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Compensation);
        app.docs.settings["compensation"]["pitch"]["enabled"] = serde_json::json!(true);
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .get_mut("pitch")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("enabled", LegacyValue::Bool(false));
            });
        });
        // The rows show the stored values as the documents read them, refreshed every main-loop pass.
        app.refresh_docs();
        let html = openrigcue_ui::ui::screens::main(&app);
        assert!(!f2_tag(&html, "c-motioncompensation-pitch").contains(" checked"));
        assert_eq!(f2_mc(&h, &["pitch", "enabled"]), LegacyValue::Bool(false));
        f2_act(&mut app, "section", "", "1").unwrap();
        assert!(
            f2_tag(&openrigcue_ui::ui::screens::main(&app), "c-motioncompensation-height").contains("value=\"1000\"")
        );
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("height", LegacyValue::Float(f64::NAN));
            });
        });
        app.refresh_docs();
        assert!(
            openrigcue_ui::ui::screens::main(&app).contains("id=\"c-motioncompensation-height\""),
            "the row still renders"
        );
        assert!(matches!(f2_mc(&h, &["height"]), LegacyValue::Float(value) if value.is_nan()));
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| raw.insert("MOTION_COMPENSATION", LegacyValue::None));
        });
        app.refresh_docs();
        f2_act(&mut app, "section", "", "0").unwrap();
        assert!(!f2_tag(&openrigcue_ui::ui::screens::main(&app), "c-motioncompensation-pitch").contains(" checked"));
        assert_eq!(
            h.context.state.config.read().settings().native_snapshot().get("MOTION_COMPENSATION"),
            Some(&LegacyValue::None)
        );
        drop(app.take_threads());
    }

    #[test]
    fn mc_selects_store_their_codes_without_control_io_and_keep_parent_failures() {
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Compensation);
        let key = "Motion Compensation";
        f2_act(&mut app, "set", &format!("{key}:installation_mode"), "0").unwrap();
        assert_eq!(f2_mc(&h, &["installation"]), LegacyValue::Str("H".into()));
        f2_act(&mut app, "set", &format!("{key}:installation_mode"), "1").unwrap();
        assert_eq!(f2_mc(&h, &["installation"]), LegacyValue::Str("V".into()));
        assert!(f2_act(&mut app, "set", &format!("{key}:installation_mode"), "7").unwrap_err().starts_with("KeyError"));
        backend
            .assign_setting_value(
                "MOTIONBOX",
                "model",
                &LegacyValue::Str(
                    openrigcue_ui::catalog::models()
                        .iter()
                        .find(|model| model.enable_intellicomp)
                        .unwrap()
                        .name
                        .clone(),
                ),
            )
            .unwrap();
        assert_eq!(f2_mc(&h, &["installation"]), LegacyValue::Str("V".into()));
        f2_act(&mut app, "set", &format!("{key}:sensor_type"), "0").unwrap();
        assert_eq!(f2_mc(&h, &["type"]), LegacyValue::Str("W".into()));
        let loop_guard = h.context.main_loop.lock().unwrap();
        let imu_guard = h.context.accessories.imu.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(f2_act(&mut app, "set", "Motion Compensation:sensor_type", "1")).unwrap();
            app
        });
        let independent = receive.recv_timeout(Duration::from_secs(2));
        drop(imu_guard);
        drop(loop_guard);
        let mut app = worker.join().unwrap();
        independent.expect("the sensor choice waited for control/device locks").unwrap();
        assert_eq!(f2_mc(&h, &["type"]), LegacyValue::Str("V".into()));
        // Calibration is offered only for the WITMotion sensor.
        let html = openrigcue_ui::ui::screens::main(&app);
        for axis in ["roll", "pitch"] {
            assert!(!html.contains(&format!("data-arg=\"{key}:{axis}:0\"")), "{axis}");
        }
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| raw.insert("MOTION_COMPENSATION", LegacyValue::None));
        });
        assert_eq!(
            f2_act(&mut app, "set", &format!("{key}:installation_mode"), "0").unwrap_err(),
            "TypeError: 'NoneType' object does not support item assignment"
        );
        assert_eq!(
            h.context.state.config.read().settings().native_snapshot().get("MOTION_COMPENSATION"),
            Some(&LegacyValue::None)
        );
        drop(app.take_threads());
    }

    #[test]
    fn mc_numbers_and_swap_store_integers_and_keep_the_assignment_failure() {
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let surge = openrigcue_ui::catalog::models()
            .iter()
            .find(|model| model.port_mapping.iter().any(|port| port == "Surge"))
            .unwrap();
        backend.assign_setting_value("MOTIONBOX", "model", &LegacyValue::Str(surge.name.clone())).unwrap();
        let mut app = mounted_app(backend, openrigcue_ui::ui::Screen::Compensation);
        let key = "Motion Compensation";
        f2_act(&mut app, "set", &format!("{key}:surge_horizontal_factor"), "23").unwrap();
        assert_eq!(f2_mc(&h, &["surge_horizontal_factor"]), LegacyValue::Int(23));
        for (row, stored) in [
            ("height", "height"),
            ("surge_stroke", "surge_horizontal_stroke"),
            ("actuator_distance_left_right", "mm_distance_left_right"),
            ("actuator_distance_front_back", "mm_distance_front_back"),
            ("vertical_actuators_stroke", "vertical_actuators_stroke"),
        ] {
            f2_act(&mut app, "set", &format!("{key}:{row}"), "23").unwrap();
            assert_eq!(f2_mc(&h, &[stored]), LegacyValue::Int(23), "{stored}");
        }
        f2_act(&mut app, "set", &format!("{key}:swap"), "1").unwrap();
        assert_eq!(f2_mc(&h, &["swap_roll_pitch"]), LegacyValue::Str("yes".into()));
        f2_act(&mut app, "set", &format!("{key}:swap"), "0").unwrap();
        assert_eq!(f2_mc(&h, &["swap_roll_pitch"]), LegacyValue::Str("no".into()));
        let html = openrigcue_ui::ui::screens::main(&app);
        assert!(f2_tag(&html, "c-motioncompensation-surge_horizontal_factor").contains("value=\"23\""));
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| raw.insert("MOTION_COMPENSATION", LegacyValue::None));
        });
        assert_eq!(
            f2_act(&mut app, "set", &format!("{key}:surge_horizontal_factor"), "33").unwrap_err(),
            "TypeError: 'NoneType' object does not support item assignment"
        );
        assert_eq!(
            h.context.state.config.read().settings().native_snapshot().get("MOTION_COMPENSATION"),
            Some(&LegacyValue::None)
        );
        drop(app.take_threads());
    }

    #[test]
    fn mc_axis_switches_store_booleans_without_control_io_and_keep_parent_failures() {
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Compensation);
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("MOTION_COMPENSATION")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .get_mut("pitch")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("enabled", LegacyValue::Float(f64::NAN));
            });
        });
        // The shortcut action toggles the stored value as it is stored: NaN is true, so the axis turns off.
        h.context.hotkeys.queue("motion_compensation_pitch_switch");
        openrigcue_ui::ui::screens::compensation::rules(&mut app).unwrap();
        assert_eq!(f2_mc(&h, &["pitch", "enabled"]), LegacyValue::Bool(false));
        h.context.hotkeys.clear_ui_actions();
        let loop_guard = h.context.main_loop.lock().unwrap();
        let imu_guard = h.context.accessories.imu.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(f2_act(&mut app, "set", "Motion Compensation:pitch_reverse", "1")).unwrap();
            app
        });
        let independent = receive.recv_timeout(Duration::from_secs(2));
        drop(imu_guard);
        drop(loop_guard);
        let mut app = worker.join().unwrap();
        independent.expect("the axis assignment waited for the control/device locks").unwrap();
        assert_eq!(f2_mc(&h, &["pitch", "reverse"]), LegacyValue::Bool(true));
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("MOTION_COMPENSATION").unwrap().as_dict_mut().unwrap().insert("pitch", LegacyValue::None);
            });
        });
        assert_eq!(
            f2_act(&mut app, "set", "Motion Compensation:pitch", "1").unwrap_err(),
            "TypeError: 'NoneType' object does not support item assignment"
        );
        assert_eq!(f2_mc(&h, &["pitch"]), LegacyValue::None);
        drop(app.take_threads());
    }

    #[test]
    fn live_mode_clicks_production_getter_exposes_all_registered_boolean_modes() {
        let h = super::super::tests::harness();
        {
            let mut controls = h.context.state.controls.write();
            controls.switches.motion = false;
            controls.switches.shakers = true;
            controls.switches.compensation = false;
            controls.switches.belt = true;
            controls.switches.sim3d = true;
            controls.switches.hud = false;
            controls.switches.iflag = true;
            controls.switches.haptic = false;
        }
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        for (field, expected) in [
            ("motion", 0),
            ("shakers", 1),
            ("compensation", 0),
            ("belt", 1),
            ("sim3d", 1),
            ("hud", 0),
            ("iflag", 1),
            ("haptic", 0),
        ] {
            assert_eq!(backend.control_value(&["switches", field]).unwrap(), LegacyValue::Int(expected));
        }
    }

    #[test]
    fn required_game_name_production_lookup_is_direct_and_keeps_native_key_errors() {
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        assert_eq!(backend.game_name_value(&"ac".into()).unwrap(), LegacyValue::Str("Assetto Corsa".into()));
        for (key, error) in [
            ("missing game".into(), "KeyError: 'missing game'"),
            (LegacyValue::Int(1), "KeyError: 1"),
            (LegacyValue::List(vec![]), "TypeError: unhashable type: 'list'"),
            (LegacyValue::Tuple(vec![LegacyValue::List(vec![])]), "TypeError: unhashable type: 'list'"),
        ] {
            assert_eq!(backend.game_name_value(&key).unwrap_err().detail, error);
        }
        assert!(matches!(&backend.runtime, Runtime::Own(runtime) if runtime.get().is_none()));
    }

    #[test]
    fn native_sidebar_values_getters_keep_raw_current_and_previous_vehicles_without_processing_lock() {
        use openrigcue_config::import::value::LegacyValue;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        {
            let mut session = h.context.state.session.write();
            session.game = Some("ac".into());
            session.set_vehicle_value(LegacyValue::Float(1.0), Some("prior projection".into()));
        }
        let mut processing = h.context.main_loop.lock().unwrap();
        processing.synchronize_profile(&h.context).unwrap();
        processing.remember_actual_vehicle(&h.context);
        {
            let mut session = h.context.state.session.write();
            session.listener = Some("ac".into());
            session.set_vehicle_value(LegacyValue::Float(2.0), Some("current projection".into()));
        }
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send((
                backend.session_value("vehicle_name"),
                backend.previous_vehicle_value(),
                backend.session_value("listener_running"),
            ))
            .unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(processing);
        worker.join().unwrap();
        let (current, previous, listener) = result.expect("native sidebar reads must not wait for profile processing");
        assert_eq!(current.unwrap(), LegacyValue::Float(2.0));
        assert_eq!(previous.unwrap(), LegacyValue::Float(1.0));
        assert_eq!(listener.unwrap(), LegacyValue::Str("ac".into()));
    }

    #[test]
    fn live_shaker_snapshot_status_does_not_wait_for_audio_processing() {
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let processing = h.context.shakers.lock();
        let mixer = processing.system.mixer();
        let audio = mixer.lock().unwrap();
        h.context.shakers.request_soundcard_refresh();
        let work = backend.clone();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || send.send(work.call(Method::Get, "status", None)).unwrap());
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(audio);
        drop(processing);
        worker.join().unwrap();
        let status = result.expect("shaker metadata must not wait for audio processing").unwrap();
        assert_eq!(status["shakers"]["output_ready"], false);
        assert_eq!(status["shakers"]["is_fmod"], false);
        assert_eq!(status["shakers"]["sample_source"], "built_in");
        assert_eq!(status["shakers"]["refresh_soundcard"], true);
    }

    #[test]
    fn live_vehicle_snapshot_status_does_not_wait_for_profile_processing() {
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let processing = h.context.main_loop.lock().unwrap();
        let work = backend.clone();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || send.send(work.call(Method::Get, "status", None)).unwrap());
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(processing);
        worker.join().unwrap();
        let status = result.expect("displayed vehicle must not wait for profile processing").unwrap();
        assert_eq!(status["displayed_vehicle"], serde_json::Value::Null);
    }

    #[test]
    fn live_accessory_snapshot_document_refresh_does_not_wait_for_device_io() {
        use openrigcue_effects::accessories::belt::BeltIdentity;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let mut intellibox = h.context.accessories.intellibox.lock().unwrap();
        let mut belt = h.context.accessories.belt.lock().unwrap();
        let mut iflag = h.context.accessories.iflag.lock().unwrap();
        let sim3d = h.context.accessories.sim3d.lock().unwrap();
        let hapticr = h.context.accessories.hapticr.lock().unwrap();
        let imu = h.context.accessories.imu.lock().unwrap();
        intellibox.set_model(17);
        intellibox.set_version(6);
        belt.set_identity(Some(BeltIdentity { model: 18, version: 19, serial: "test-belt".into() }));
        iflag.set_firmware(Some("1.3".into()));
        let work = backend.clone();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || send.send(openrigcue_ui::app::read_docs(work.as_ref())).unwrap());
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop((intellibox, belt, iflag, sim3d, hapticr, imu));
        worker.join().unwrap();
        let documents = result.expect("the UI document refresh must not wait for accessory I/O");
        let mut app = openrigcue_ui::app::MyApp::new(backend);
        app.apply_docs(documents);
        assert_eq!(app.status(&["accessories", "intellibox_model"]), &serde_json::json!(17));
        assert_eq!(app.status(&["accessories", "intellibox_version"]), &serde_json::json!(6));
        assert_eq!(app.status(&["accessories", "belt_identity", "serial"]), &serde_json::json!("test-belt"));
        // The status publishes the `iflag_version` number as stored (see `accessory_status`).
        assert_eq!(app.status(&["accessories", "iflag_firmware"]), &serde_json::json!(1.3));
    }

    #[test]
    fn live_belt_homing_control_writes_do_not_wait_for_device_io() {
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let belt = h.context.accessories.belt.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(backend.call(Method::Patch, "controls", Some(serde_json::json!({"tests":{"belt_homing":2}}))))
                .unwrap()
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(belt);
        worker.join().unwrap();
        result.expect("global homing assignment must not wait for belt I/O").unwrap();
        assert_eq!(h.context.accessories.belt_homing(), openrigcue_effects::accessories::belt::Homing::Requested);
    }

    /// The app on Hardware → Hardware with the PTA Motion System as the motion model and `udp_config` as its
    /// stored UDP destinations, its dialog opened through the motion model row's Controller Settings button.
    fn f3_pta_app(
        h: &super::super::tests::Harness,
        udp_config: &LegacyValue,
    ) -> (Arc<DaemonBackend>, openrigcue_ui::app::MyApp) {
        use openrigcue_ui::ui::dialogs_hardware::{device, Device};
        let backend = harness_backend(h);
        backend.assign_setting_value("MOTIONBOX", "model", &LegacyValue::Str("PTA Motion System".into())).unwrap();
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Hardware);
        backend.assign_setting_value("PT-ACTUATOR", "udp_config", udp_config).unwrap();
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_model:2", "").unwrap();
        assert!(matches!(device(&app), Some(Device::Pta { .. })), "the PTA Motion System dialog");
        (backend, app)
    }

    #[test]
    fn native_pta_dialog_uses_the_native_destination_setting() {
        use openrigcue_ui::ui::dialogs_hardware::{device, Device};
        let h = super::super::tests::harness();
        // A stored `None`: the dialog shows no destination, Add cannot append to it and nothing is written.
        let (backend, mut app) = f3_pta_app(&h, &LegacyValue::None);
        let Some(Device::Pta { list, attached, .. }) = device(&app) else { panic!("PTA dialog") };
        assert_eq!((list, *attached), (&LegacyValue::None, true), "the dialog holds the stored value itself");
        assert_eq!(
            openrigcue_ui::ui::dispatch(&mut app, "dialog-button", "add", "").unwrap_err(),
            "AttributeError: 'NoneType' object has no attribute 'append'"
        );
        assert_eq!(backend.native_setting("PT-ACTUATOR", "udp_config").unwrap(), LegacyValue::None);
        drop(app.take_threads());
        // A stored list: the dialog shows its rows, read from the same setting.
        let stored = LegacyValue::List(vec![LegacyValue::Str("10.0.0.1:7408".into())]);
        let (backend, mut app) = f3_pta_app(&h, &stored);
        let html = openrigcue_ui::ui::dialogs::render(&app);
        assert!(html.contains("10.0.0.1") && html.contains("7408"), "{html}");
        assert_eq!(backend.native_setting("PT-ACTUATOR", "udp_config").unwrap(), stored, "opening writes nothing");
        drop(app.take_threads());
    }

    #[test]
    fn intellibox_test_steps_led_and_tach_getters_return_current_parameter_codes() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        {
            let mut controls = h.context.state.controls.write();
            controls.switches.led = crate::runtime::OutputMode::AlwaysOn;
            controls.switches.tach = crate::runtime::OutputMode::Off;
        }
        assert_eq!(backend.control_value(&["switches", "led"]).unwrap(), LegacyValue::Int(2));
        assert_eq!(backend.control_value(&["switches", "tach"]).unwrap(), LegacyValue::Int(0));
    }

    #[test]
    fn native_soundcard_callback_missing_parent_stops_before_the_device_operation() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Hardware);
        // The Sound Device list: index 0 is "None".
        app.ui.devices.hardware.listing = Some(serde_json::json!({"sound_cards": ["None", "Speakers"]}));
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.remove("SHAKEBOX");
            });
        });
        let opened = app.backend.translate("Could not open the selected sound device: ^error^");
        let opened = opened.split("^error^").next().unwrap().to_owned();
        assert_eq!(
            openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:soundcard", "0").unwrap_err(),
            "KeyError: 'SHAKEBOX'"
        );
        assert!(h.context.state.config.read().settings().native_snapshot().get("SHAKEBOX").is_none());
        assert!(
            !app.ui.notices.history.iter().any(|notice| notice.message.starts_with(&opened)),
            "the failed write stops before the sound device is opened"
        );
        drop(app.take_threads());
    }

    #[test]
    fn native_shaker_dialog_confirmation_keeps_nan_ties_and_unknown_native_fields() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let mut levels =
            backend.shaker_effects().unwrap().item_owned("SUB").unwrap().item_owned("sfx").unwrap().clone();
        levels.assign_item("accel_lr", LegacyValue::Float(f64::NAN)).unwrap();
        levels.assign_item("accel_rr", LegacyValue::Int(8)).unwrap();
        levels.assign_item("brake_lf", LegacyValue::Bool(true)).unwrap();
        levels.assign_item("brake_rf", LegacyValue::Float(1.0)).unwrap();
        let extra = LegacyValue::Tuple(vec![LegacyValue::Bytes(b"kept".to_vec())]);
        levels.assign_item("unknown", extra.clone()).unwrap();
        let stored_before =
            h.context.state.config.read().metadata_row("shakers", &h.context.state.shaker_layout.read().name).unwrap();
        backend.confirm_shaker_levels(Some("SUB"), &levels).unwrap();
        let loaded = backend.shaker_effects().unwrap();
        let result = loaded.item_owned("SUB").unwrap().item_owned("sfx").unwrap();
        assert!(matches!(result.item_owned("accel").unwrap(), LegacyValue::Float(value) if value.is_nan()));
        assert_eq!(result.item_owned("brake").unwrap(), LegacyValue::Bool(true));
        assert_eq!(result.item_owned("unknown").unwrap(), extra);
        assert_eq!(
            h.context.state.config.read().metadata_row("shakers", &h.context.state.shaker_layout.read().name).unwrap(),
            stored_before
        );
    }

    #[test]
    fn native_shaker_dialog_bad_comparison_keeps_the_copy_without_deriving_groups() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let before = h.context.shakers.lock().system.levels().clone();
        for (first, second) in [
            (LegacyValue::Int(1), LegacyValue::Str("2".into())),
            (LegacyValue::List(vec![LegacyValue::Int(1)]), LegacyValue::List(vec![LegacyValue::Str("2".into())])),
        ] {
            let mut levels = LegacyValue::from(serde_json::json!({"opaque":9}));
            levels.assign_item("accel_lr", first).unwrap();
            levels.assign_item("accel_rr", second).unwrap();
            assert_eq!(
                backend.confirm_shaker_levels(Some("SUB"), &levels).unwrap_err().detail,
                "TypeError: '>' not supported between instances of 'str' and 'int'"
            );
            assert_eq!(backend.shaker_effects().unwrap().item_owned("SUB").unwrap().item_owned("sfx").unwrap(), levels);
            assert_eq!(h.context.shakers.lock().system.levels(), &before);
        }
    }

    #[test]
    fn native_shaker_dialog_success_updates_in_memory_channel_groups() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let before = h.context.shakers.lock().system.levels().clone();
        let mut levels =
            backend.shaker_effects().unwrap().item_owned("SUB").unwrap().item_owned("sfx").unwrap().clone();
        levels.assign_item("engine", LegacyValue::Int(3)).unwrap();
        backend.confirm_shaker_levels(Some("SUB"), &levels).unwrap();
        let expected = crate::shakers::shaker_settings(&h.context.state);
        assert_ne!(h.context.shakers.lock().system.levels(), &before);
        assert_eq!(
            h.context.shakers.lock().system.levels(),
            &openrigcue_effects::shakers::levels::GroupLevels::compute(
                expected.profile.as_ref().unwrap(),
                &expected.tuning,
                None
            )
        );
    }

    #[test]
    fn native_shaker_dialog_confirmation_publishes_copy_and_accel_before_missing_brake() {
        use openrigcue_ui::ui::dialogs_hardware::{device, Device};
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Hardware);
        let mut layout =
            LegacyValue::from(serde_json::json!({"SUB":{"sfx":{"engine":4,"accel_lr":true,"accel_rr":1.0}}}));
        h.context.state.shaker_layout.write().value = layout.clone();
        // The SUB row's Config button: the dialog edits a copy of the speaker's stored levels.
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:sub:1", "").unwrap();
        let Some(Device::Matrix { speaker, levels }) = device(&app) else { panic!("speaker matrix dialog") };
        assert_eq!(speaker, "SUB");
        assert_eq!(levels, &layout.item_owned("SUB").unwrap().item_owned("sfx").unwrap());
        layout
            .as_dict_mut()
            .unwrap()
            .get_mut("SUB")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .get_mut("sfx")
            .unwrap()
            .assign_item("late_global_edit", LegacyValue::Int(9))
            .unwrap();
        h.context.state.shaker_layout.write().value = layout;
        assert_eq!(openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "").unwrap_err(), "KeyError: 'brake_lf'");
        assert!(device(&app).is_none(), "the dialog closes before the confirmation");
        let loaded = backend.shaker_effects().unwrap();
        let levels = loaded.item_owned("SUB").unwrap().item_owned("sfx").unwrap();
        assert_eq!(levels.item_owned("accel").unwrap(), LegacyValue::Bool(true));
        assert!(levels.item_owned("late_global_edit").is_err(), "the copy taken at opening is published");
        assert!(levels.item_owned("brake").is_err());
        drop(app.take_threads());
    }

    #[test]
    fn native_pta_dialog_tuple_confirmation_keeps_kind_and_add_has_no_append() {
        use openrigcue_ui::ui::dialogs_hardware::device;
        let h = super::super::tests::harness();
        let tuple = LegacyValue::Tuple(vec![LegacyValue::Str("10.0.0.1:7408".into())]);
        let (backend, mut app) = f3_pta_app(&h, &tuple);
        assert_eq!(
            openrigcue_ui::ui::dispatch(&mut app, "dialog-button", "add", "").unwrap_err(),
            "AttributeError: 'tuple' object has no attribute 'append'"
        );
        assert_eq!(backend.native_setting("PT-ACTUATOR", "udp_config").unwrap(), tuple);
        assert!(device(&app).is_some(), "the failed Add keeps the dialog open");
        openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert!(device(&app).is_none());
        assert_eq!(
            backend.native_setting("PT-ACTUATOR", "udp_config").unwrap(),
            tuple,
            "OK stores the tuple as a tuple"
        );
        drop(app.take_threads());
    }

    #[test]
    fn native_pta_dialog_bytes_row_is_shown_and_kept_as_bytes() {
        let h = super::super::tests::harness();
        let stored = LegacyValue::List(vec![LegacyValue::Bytes(b"10.0.0.1:7408".to_vec())]);
        let (backend, mut app) = f3_pta_app(&h, &stored);
        let html = openrigcue_ui::ui::dialogs::render(&app);
        assert!(html.contains("10.0.0.1"), "{html}");
        assert_eq!(backend.native_setting("PT-ACTUATOR", "udp_config").unwrap(), stored, "opening writes nothing");
        openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert_eq!(backend.native_setting("PT-ACTUATOR", "udp_config").unwrap(), stored, "the row stays bytes");
        drop(app.take_threads());
    }

    #[test]
    fn native_thanos_apply_all_empty_database_does_not_read_recommendation() {
        let h = super::super::tests::harness();
        let mut app = apply_all_app(&h);
        {
            let mut store = h.context.state.config.write();
            let rows = store.profile_rows().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
            for (game, _) in rows {
                store.stage_game_row(&game, None).unwrap();
            }
            store.commit_profiles().unwrap();
        }
        openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "").unwrap();
        let done = app.backend.translate("Every game / vehicle now uses the new max angle");
        assert!(app.ui.notices.history.iter().any(|notice| notice.message == done));
        assert!(matches!(
            openrigcue_ui::ui::dialogs_hardware::device(&app),
            Some(openrigcue_ui::ui::dialogs_hardware::Device::Thanos(_))
        ));
    }

    #[test]
    fn native_thanos_apply_all_bad_recommendation_propagates_after_alert_creation() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let mut app = apply_all_app(&h);
        {
            let mut store = h.context.state.config.write();
            store.stage_game_row("ac", Some(LegacyValue::from(serde_json::json!({"opaque": "keep"})))).unwrap();
            store.commit_profiles().unwrap();
        }
        let result = openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "");
        assert_eq!(result.unwrap_err(), "KeyError: 'vehicles'");
        let done = app.backend.translate("Every game / vehicle now uses the new max angle");
        assert!(!app.ui.notices.history.iter().any(|notice| notice.message == done));
    }

    fn seed_apply_all_profiles(
        h: &super::super::tests::Harness,
        rows: &[(&str, openrigcue_model::native_value::LegacyValue)],
    ) -> Arc<DaemonBackend> {
        let mut store = h.context.state.config.write();
        let old = store.profile_rows().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        for (game, _) in old {
            store.stage_game_row(&game, None).unwrap();
        }
        for (game, value) in rows {
            store.stage_game_row(game, Some(value.clone())).unwrap();
        }
        store.commit_profiles().unwrap();
        drop(store);
        Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        })
    }

    #[test]
    fn native_thanos_apply_all_reads_each_assignment_and_skips_emulator() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let game = LegacyValue::from(serde_json::json!({"vehicles":{"car":{}},"opaque":"keep"}));
        let emulator = LegacyValue::from(serde_json::json!({"vehicles":null}));
        let backend = seed_apply_all_profiles(&h, &[("emulator", emulator.clone()), ("ac", game)]);
        let mut count = 0;
        backend
            .apply_all_tunes(true, &mut || {
                count += 1;
                Ok(10 + count)
            })
            .unwrap();
        assert_eq!(count, 4);
        let value = h.context.state.config.read().profile_row("ac").unwrap().unwrap();
        assert_eq!(value.item_owned("thanos_spike_filter_horizontal").unwrap(), LegacyValue::Int(11));
        assert_eq!(value.item_owned("thanos_spike_filter_vertical").unwrap(), LegacyValue::Int(12));
        let car = value.item_owned("vehicles").unwrap().item_owned("car").unwrap();
        assert_eq!(car.item_owned("thanos_spike_filter_horizontal").unwrap(), LegacyValue::Int(13));
        assert_eq!(car.item_owned("thanos_spike_filter_vertical").unwrap(), LegacyValue::Int(14));
        assert_eq!(h.context.state.config.read().profile_row("emulator").unwrap(), Some(emulator));
    }

    #[test]
    fn native_thanos_apply_all_getter_error_precedes_bad_vehicle_assignment_and_retains_earlier_row() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let first = LegacyValue::from(serde_json::json!({"vehicles":{}}));
        let second = LegacyValue::from(serde_json::json!({"vehicles":{"bad":null}}));
        let backend = seed_apply_all_profiles(&h, &[("first", first), ("second", second.clone())]);
        let mut count = 0;
        let result = backend.apply_all_tunes(false, &mut || {
            count += 1;
            if count == 3 {
                Err("ValueError: fixture getter failed".into())
            } else {
                Ok(16 + count)
            }
        });
        assert_eq!(result.unwrap_err().detail, "ValueError: fixture getter failed");
        let store = h.context.state.config.read();
        assert_eq!(
            store.profile_row("first").unwrap().unwrap().item_owned("motion_max_power").unwrap(),
            LegacyValue::Int(17)
        );
        assert_eq!(store.profile_row("second").unwrap(), Some(second));
    }

    #[test]
    fn native_thanos_apply_all_missing_vehicles_raises_after_the_recommendation_read() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let original = LegacyValue::from(serde_json::json!({"opaque":"keep"}));
        let backend = seed_apply_all_profiles(&h, &[("ac", original.clone())]);
        let mut count = 0;
        let result = backend.apply_all_tunes(false, &mut || {
            count += 1;
            Ok(17)
        });
        assert_eq!(result.unwrap_err().detail, "KeyError: 'vehicles'");
        assert_eq!(count, 1);
        assert_eq!(h.context.state.config.read().profile_row("ac").unwrap(), Some(original));
    }

    /// The app on Hardware → Hardware with a Thanos model: its controller dialog open (through the motion model
    /// row's Controller Settings button) and the confirmation of "Apply Max Angle to all Games / Vehicles".
    fn apply_all_app(h: &super::super::tests::Harness) -> openrigcue_ui::app::MyApp {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_ui::ui::dialogs_hardware::{device, Device};
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let model = openrigcue_ui::catalog::models().iter().find(|model| model.manufacturer == "THANOS").unwrap();
        backend.assign_setting_value("MOTIONBOX", "model", &LegacyValue::Str(model.name.clone())).unwrap();
        backend.assign_setting_value("THANOS", "lateral_distance_actuators", &LegacyValue::Int(558)).unwrap();
        backend.assign_setting_value("THANOS", "vertical_actuators_stroke", &LegacyValue::Int(150)).unwrap();
        let mut app = hardware_app(backend);
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_model:2", "").unwrap();
        let Some(Device::Thanos(dialog)) = device(&app) else { panic!("Thanos dialog") };
        assert_eq!(dialog.max_angle, 37, "558 / 150 × 10");
        openrigcue_ui::ui::dispatch(&mut app, "dialog-button", "apply:angle", "").unwrap();
        let Some(Device::Thanos(dialog)) = device(&app) else { panic!("Thanos dialog") };
        assert_eq!(dialog.confirm, Some(false), "the max angle confirmation");
        app
    }

    /// The app after the page load, on Hardware → Hardware of the interface.
    fn hardware_app(backend: Arc<DaemonBackend>) -> openrigcue_ui::app::MyApp {
        use openrigcue_ui::remi::gui::dispatch;
        let mut app = openrigcue_ui::app::MyApp::new(backend);
        let body = app.gui.create("body", "BODY");
        let root = app.main(body);
        app.gui.set_root(root);
        dispatch(&mut app, body, "onload", &Default::default());
        openrigcue_ui::ui::navigate(&mut app, openrigcue_ui::ui::Screen::Hardware, 0);
        app
    }

    #[test]
    fn native_thanos_confirmation_publishes_pending_before_a_model_error() {
        use openrigcue_devices::motion::thanos::{
            protocol::{Field, FieldValue},
            Role,
        };
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_ui::ui::dialogs_hardware::{device, Device};
        let h = super::super::tests::harness();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let model = openrigcue_ui::catalog::models().iter().find(|m| m.manufacturer == "THANOS").unwrap();
        backend.assign_setting_value("MOTIONBOX", "model", &LegacyValue::Str(model.name.clone())).unwrap();
        let mut app = hardware_app(backend.clone());
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_model:2", "").unwrap();
        assert!(matches!(device(&app), Some(Device::Thanos(_))), "open Thanos dialog");
        h.context.motion.driver_settings().thanos_shared.update(|shared| {
            shared.controller.queue(Role::Main, Field::MotorNumber, FieldValue::Int(7));
            shared.controller.queue(Role::Secondary, Field::MotorNumber, FieldValue::Int(8));
        });
        backend.assign_setting_value("MOTIONBOX", "model", &LegacyValue::Str("missing model".into())).unwrap();
        let loop_guard = h.context.main_loop.lock().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "")).unwrap();
        });
        let result = receive.recv_timeout(std::time::Duration::from_secs(2));
        drop(loop_guard);
        worker.join().unwrap();
        let result = result.expect("pending publication must not wait for the main-loop lock");
        let controller = h.context.motion.driver_settings().thanos_shared.get().controller;
        assert!(controller.main_pending.is_empty());
        assert!(controller.secondary_pending.is_empty());
        assert_eq!(result.unwrap_err(), "KeyError: 'missing model'");
    }

    #[test]
    fn native_thanos_calculator_preserves_callback_bytes_and_requires_the_original_parent() {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_ui::ui::dialogs_hardware::{device, Device};
        let h = super::super::tests::harness();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let model = openrigcue_ui::catalog::models().iter().find(|m| m.manufacturer == "THANOS").unwrap();
        backend.assign_setting_value("MOTIONBOX", "model", &LegacyValue::Str(model.name.clone())).unwrap();
        backend.assign_setting_value("THANOS", "lateral_distance_actuators", &LegacyValue::Int(558)).unwrap();
        backend.assign_setting_value("THANOS", "vertical_actuators_stroke", &LegacyValue::Int(150)).unwrap();
        let mut app = hardware_app(backend.clone());
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_model:2", "").unwrap();
        // The calculator's distance is stored as its box takes it (a whole number), in PT-ACTUATOR, and the
        // recommendation follows it.
        openrigcue_ui::ui::dispatch(&mut app, "dialog-field", "lateral", "005").unwrap();
        assert_eq!(backend.native_setting("PT-ACTUATOR", "lateral_distance_actuators").unwrap(), LegacyValue::Int(5));
        let Some(Device::Thanos(dialog)) = device(&app) else { panic!("Thanos dialog") };
        assert_eq!(dialog.max_angle, 0, "5 / 150 × 10");
        h.context.state.config.write().update(|config| {
            config.settings.edit_native(|settings| {
                settings.remove("PT-ACTUATOR");
            })
        });
        assert_eq!(
            openrigcue_ui::ui::dispatch(&mut app, "dialog-field", "lateral", "17").unwrap_err(),
            "KeyError: 'PT-ACTUATOR'"
        );
    }

    #[test]
    fn native_vnm_dialog_keeps_the_reverse_switch_pending_until_ok_and_requires_the_original_parent() {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_ui::ui::dialogs_hardware::{device, Device};
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let model = openrigcue_ui::catalog::models()
            .iter()
            .filter(|m| m.manufacturer == "VNM SIMULATION")
            .max_by_key(|m| m.port_mapping.len())
            .unwrap();
        backend.assign_setting_value("MOTIONBOX", "model", &LegacyValue::Str(model.name.clone())).unwrap();
        let mut app = hardware_app(backend.clone());
        // The motion model row's VNM Controller Settings button.
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_model:4", "").unwrap();
        assert!(matches!(device(&app), Some(Device::Vnm { .. })), "VNM dialog");
        let before = backend.native_setting("VNM SIMULATION", "reverse_controller_port_5").unwrap();
        let on = !before.is_truthy();
        openrigcue_ui::ui::dispatch(&mut app, "dialog-field", "reverse:5", if on { "1" } else { "0" }).unwrap();
        assert_eq!(
            backend.native_setting("VNM SIMULATION", "reverse_controller_port_5").unwrap(),
            before,
            "kept until OK"
        );
        let Some(Device::Vnm { reverse, .. }) = device(&app) else { panic!("VNM dialog") };
        assert!(reverse.contains(&(5, on)), "the pending switch: {reverse:?}");
        openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert!(device(&app).is_none(), "OK closes the dialog");
        assert_eq!(
            backend.native_setting("VNM SIMULATION", "reverse_controller_port_5").unwrap(),
            LegacyValue::Bool(on)
        );
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_model:4", "").unwrap();
        h.context.state.config.write().update(|config| {
            config.settings.edit_native(|settings| {
                settings.remove("VNM SIMULATION");
            })
        });
        assert_eq!(
            openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "").unwrap_err(),
            "KeyError: 'VNM SIMULATION'"
        );
        drop(app.take_threads());
    }

    #[test]
    fn native_dof_dialog_preserves_pending_bytes_and_requires_the_original_parent() {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_ui::ui::dialogs_hardware::{device, Device};
        let h = super::super::tests::harness();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let mut app = hardware_app(backend.clone());
        // The motion model row's DOF Reality Controller Settings button.
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_model:5", "").unwrap();
        assert!(matches!(device(&app), Some(Device::Dof { .. })), "DOF dialog");
        let before = backend.native_setting("DOF REALITY", "update_kp").unwrap();
        let on = !before.is_truthy();
        openrigcue_ui::ui::dispatch(&mut app, "dialog-field", "update_kp", if on { "1" } else { "0" }).unwrap();
        assert_eq!(backend.native_setting("DOF REALITY", "update_kp").unwrap(), before, "kept until OK");
        openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert_eq!(backend.native_setting("DOF REALITY", "update_kp").unwrap(), LegacyValue::Bool(on));
        h.context.state.config.write().update(|config| {
            config.settings.edit_native(|settings| {
                settings.remove("DOF REALITY");
            })
        });
        assert_eq!(
            openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_model:5", "").unwrap_err(),
            "KeyError: 'DOF REALITY'"
        );
    }

    #[test]
    fn native_yaw_limits_confirmation_stores_integer_settings_through_the_real_modal() {
        use openrigcue_devices::motion::yaw_vr::protocol::YawLimits;
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_ui::ui::dialogs_hardware::{device, Device};
        let h = super::super::tests::harness();
        h.context.motion.driver_settings().yaw_shared.update(|shared| shared.limits = Some(YawLimits::EMULATOR));
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let mut app = hardware_app(backend.clone());
        // The motion model row's Yaw VR button.
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_model:1", "").unwrap();
        assert!(matches!(device(&app), Some(Device::YawVr { .. })), "Yaw VR dialog");
        openrigcue_ui::ui::dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert!(matches!(backend.native_setting("Yaw VR", "auto_park").unwrap(), LegacyValue::Int(_)));
        assert!(matches!(backend.native_setting("Yaw VR", "center_before_start_alert").unwrap(), LegacyValue::Int(_)));
        assert!(matches!(backend.native_setting("Yaw VR", "6dof_mode").unwrap(), LegacyValue::Int(_)));
    }

    #[test]
    fn native_yaw_limit_switches_store_bytes_and_require_the_original_parent() {
        use openrigcue_devices::motion::yaw_vr::protocol::YawLimits;
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        h.context.motion.driver_settings().yaw_shared.update(|shared| shared.limits = Some(YawLimits::EMULATOR));
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let mut app = hardware_app(backend.clone());
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_model:1", "").unwrap();
        // Auto Park is stored as soon as it is switched.
        openrigcue_ui::ui::dispatch(&mut app, "dialog-field", "yaw:auto_park", "0").unwrap();
        assert_eq!(backend.native_setting("Yaw VR", "auto_park").unwrap(), LegacyValue::Bool(false));
        h.context.state.config.write().update(|config| {
            config.settings.edit_native(|settings| {
                settings.remove("Yaw VR");
            })
        });
        let error = openrigcue_ui::ui::dispatch(&mut app, "dialog-field", "yaw:auto_park", "1").unwrap_err();
        assert_eq!(error, "KeyError: 'Yaw VR'");
    }

    #[test]
    fn native_yaw_limits_dialog_has_current_parameters_without_ui_or_device_io_locks() {
        use openrigcue_devices::motion::yaw_vr::protocol::YawLimits;
        use openrigcue_model::native_value::LegacyValue;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        h.context.motion.driver_settings().yaw_shared.update(|shared| {
            shared.device_model = "YAWDEVICE".into();
            shared.limits = Some(YawLimits {
                power: 17,
                pitch_forward: 10,
                pitch_backward: 11,
                roll: 12,
                yaw: 90,
                yaw_limit_enabled: 1,
                vibration_enabled: 0,
            });
        });
        let main_loop = h.context.main_loop.lock().unwrap();
        let device = h.context.accessories.iflag.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || send.send(backend.motion_parameter("Yaw VR")).unwrap());
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop((main_loop, device));
        worker.join().unwrap();
        let parameters = result.expect("Yaw parameters must avoid UI and accessory I/O locks").unwrap();
        assert_eq!(parameters.item_owned("power").unwrap(), LegacyValue::Int(17));
        assert_eq!(parameters.item_owned("device_model").unwrap(), LegacyValue::Str("YAWDEVICE".into()));
    }

    #[test]
    fn native_belt_test_scalar_is_stored_in_the_runtime_and_kept_by_other_control_writes() {
        use openrigcue_model::native_value::LegacyValue;
        use serde_json::json;
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = hardware_app(backend.clone());
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:belt_test_slider", "17").unwrap();
        assert_eq!(backend.control_value(&["tests", "belt_test_value"]).unwrap(), LegacyValue::Int(17));
        assert_eq!(
            app.control(&["tests", "belt_test_value"]).as_f64(),
            Some(17.0),
            "the screen reads the stored value"
        );
        backend.call(Method::Patch, "controls", Some(json!({"switches":{"motion":false}}))).unwrap();
        assert_eq!(backend.control_value(&["tests", "belt_test_value"]).unwrap(), LegacyValue::Int(17));
        // A value of another kind assigned in the runtime stays as it is.
        let raw = LegacyValue::Bytes(b"17".to_vec());
        backend.assign_belt_test_value(&raw, 17.0).unwrap();
        backend.call(Method::Patch, "controls", Some(json!({"switches":{"motion":false}}))).unwrap();
        assert_eq!(backend.control_value(&["tests", "belt_test_value"]).unwrap(), raw);
        backend.call(Method::Patch, "controls", Some(json!({"tests":{"belt_test_value":18}}))).unwrap();
        assert_eq!(backend.control_value(&["tests", "belt_test_value"]).unwrap(), LegacyValue::Int(18));
        drop(app.take_threads());
    }

    #[test]
    fn native_motion_test_scalar_is_stored_in_the_runtime_and_reset_by_the_test_toggle() {
        use openrigcue_model::native_value::LegacyValue;
        use serde_json::json;
        let h = super::super::tests::harness();
        h.context.state.config.write().update(|configuration| configuration.settings.motion.model = "P3".into());
        let backend = harness_backend(&h);
        let mut app = hardware_app(backend.clone());
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:pitch", "17").unwrap();
        assert_eq!(backend.control_value(&["tests", "motion_sliders", "pitch"]).unwrap(), LegacyValue::Int(17));
        assert_eq!(
            app.control(&["tests", "motion_sliders", "pitch"]).as_f64(),
            Some(17.0),
            "the screen reads the stored value"
        );
        backend.call(Method::Patch, "controls", Some(json!({"switches":{"motion":false}}))).unwrap();
        assert_eq!(backend.control_value(&["tests", "motion_sliders", "pitch"]).unwrap(), LegacyValue::Int(17));
        assert!(h.context.motion.test_positions(&h.context.state).unwrap().is_some());
        assert_eq!(
            openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:pitch", "x").unwrap_err(),
            "ValueError: not a number: \"x\""
        );
        assert_eq!(backend.control_value(&["tests", "motion_sliders", "pitch"]).unwrap(), LegacyValue::Int(17));
        // A value of another kind assigned in the runtime stays as it is, and the position computation rejects it.
        let value = LegacyValue::Bytes(b"17".to_vec());
        backend.assign_motion_test_value("pitch", &value, 17.0).unwrap();
        assert_eq!(backend.control_value(&["tests", "motion_sliders", "pitch"]).unwrap(), value);
        let error = h.context.motion.test_positions(&h.context.state).unwrap_err();
        assert_eq!(error.exception(), openrigcue_cueing::Exception::TypeError);
        assert_eq!(error.message(), "unsupported operand type(s) for /: 'bytes' and 'float'");
        backend.call(Method::Patch, "controls", Some(json!({"tests":{"motion_sliders":{"pitch":18}}}))).unwrap();
        assert_eq!(backend.control_value(&["tests", "motion_sliders", "pitch"]).unwrap(), LegacyValue::Int(18));
        assert!(h.context.motion.test_positions(&h.context.state).unwrap().is_some());
        // Start Test: the test turns on and every test slider goes back to 0.
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_test", "").unwrap();
        assert!(h.context.state.controls.read().tests.motion_test);
        assert_eq!(backend.control_value(&["tests", "motion_sliders", "pitch"]).unwrap(), LegacyValue::Int(0));
        drop(app.take_threads());
    }

    #[test]
    fn live_measurement_values_have_a_current_actuator_position_accessor() {
        use openrigcue_model::native_value::LegacyValue;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        assert_eq!(
            backend.motion_parameter("actuators_position").unwrap().item_owned("front_left").unwrap(),
            LegacyValue::Int(0)
        );
        h.context.state.motion.write().actuators = Some(openrigcue_devices::motion::ActuatorPositions {
            front_left: 17,
            front_right: 18,
            rear_left: 19,
            rear_right: 20,
            motors: [1, 2, 3, 4, 5, 6],
        });
        let main_loop = h.context.main_loop.lock().unwrap();
        let device = h.context.accessories.iflag.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || send.send(backend.motion_parameter("actuators_position")).unwrap());
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop((main_loop, device));
        worker.join().unwrap();
        let values = result.expect("motor reads must avoid UI-loop and accessory I/O locks").unwrap();
        assert_eq!(values.item_owned("front_left").unwrap(), LegacyValue::Int(17));
        for index in 0..6 {
            assert_eq!(values.item_owned(&format!("m{index}")).unwrap(), LegacyValue::Int(index + 1));
        }
    }

    #[test]
    fn live_motion_test_callback_uses_the_current_mode_before_its_measurement_flag() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let mut app = hardware_app(harness_backend(&h));
        h.context.state.controls.write().tests.motion_test = true;
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("CUSTOM_FEATURES")
                    .unwrap()
                    .assign_item("enable_motion_measurement_mode", LegacyValue::None)
                    .unwrap();
            });
        });
        // Stop Test: the current mode decides, so the unreadable measurement flag is never read.
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:motion_test", "").unwrap();
        assert!(!h.context.state.controls.read().tests.motion_test);
        let html = (0..16)
            .map(|section| {
                app.ui.section = section;
                openrigcue_ui::ui::screens::main(&app)
            })
            .find(|html| html.contains("data-control=\"motion_test\""))
            .expect("the motion test row");
        let row = html.split("data-control=\"motion_test\"").nth(1).unwrap();
        assert!(row.contains("Start Test"), "the button offers Start Test again");
        assert!(app.ui.devices.hardware.sweep.is_none(), "no measurement sweep starts");
        assert_eq!(
            h.context
                .state
                .config
                .read()
                .settings()
                .native_snapshot()
                .get("CUSTOM_FEATURES")
                .unwrap()
                .item_owned("enable_motion_measurement_mode")
                .unwrap(),
            LegacyValue::None
        );
        drop(app.take_threads());
    }

    #[test]
    fn native_axis_dialog_actuators_have_a_current_parameter_accessor() {
        use openrigcue_model::native_value::{Dict, Key, LegacyValue};
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        assert_eq!(backend.motion_parameter("actuators").unwrap(), LegacyValue::Dict(Dict::new()));
        h.context.state.motion.write().assignment = Some(openrigcue_devices::motion::AxisAssignment {
            actuators: vec![openrigcue_devices::motion::AssignableActuator {
                key: 7,
                axis: "1".into(),
                com_port: "COM17".into(),
                sn: "77".into(),
            }],
            status: true,
            tl_port: String::new(),
        });
        let main_loop = h.context.main_loop.lock().unwrap();
        let device = h.context.accessories.iflag.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || send.send(backend.motion_parameter("actuators")).unwrap());
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop((main_loop, device));
        worker.join().unwrap();
        let values = result.expect("the current view read must avoid UI and device I/O locks").unwrap();
        let row = values.as_dict().unwrap().get_key(&Key::Int(7)).unwrap();
        assert_eq!(row.item_owned("axis").unwrap(), LegacyValue::Str("1".into()));
        assert_eq!(row.item_owned("com_port").unwrap(), LegacyValue::Str("COM17".into()));
        assert_eq!(row.item_owned("sn").unwrap(), LegacyValue::Str("77".into()));
    }

    #[test]
    fn live_motion_tl_port_has_its_own_current_parameter() {
        use openrigcue_model::native_value::LegacyValue;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        assert_eq!(backend.motion_parameter("prs200_snc6_tl_com_port").unwrap(), LegacyValue::Str(String::new()));
        h.context.motion.driver_settings().prs200_tl_port.set("COM7".into());
        let main_loop = h.context.main_loop.lock().unwrap();
        let view = h.context.state.motion.write();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(backend.motion_parameter("prs200_snc6_tl_com_port")).unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop((main_loop, view));
        worker.join().unwrap();
        assert_eq!(
            result.expect("the live port must not acquire UI or snapshot locks").unwrap(),
            LegacyValue::Str("COM7".into())
        );
    }

    #[test]
    fn live_motion_ports_read_current_claims_without_ui_or_device_io_locks() {
        use openrigcue_model::native_value::LegacyValue;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let mut device = h.context.accessories.iflag.lock().unwrap();
        device.set_port("COM17".into());
        let main_loop = h.context.main_loop.lock().unwrap();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || send.send(backend.accessory_status("iflag_port")).unwrap());
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(main_loop);
        drop(device);
        worker.join().unwrap();
        assert_eq!(
            result.expect("reading a port must avoid both I/O locks").unwrap(),
            LegacyValue::Str("COM17".into())
        );
    }

    #[test]
    fn accessory_parameter_is_current_while_main_loop_and_device_io_are_locked() {
        use openrigcue_effects::accessories::status::{Connection, DeviceStatus};
        use openrigcue_model::native_value::LegacyValue;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let mut device = h.context.accessories.iflag.lock().unwrap();
        device.set_connection(Connection::Offline(DeviceStatus::Progress("probing".into())));
        let main_loop = h.context.main_loop.lock().unwrap();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || send.send(backend.accessory_status("iflag")).unwrap());
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(main_loop);
        drop(device);
        worker.join().unwrap();
        assert_eq!(
            result.expect("the parameter read must not acquire either I/O lock").unwrap(),
            LegacyValue::Str("probing".into())
        );
    }

    #[test]
    fn hardware_controls_are_current_while_main_loop_and_belt_io_are_locked() {
        use openrigcue_effects::accessories::belt::Homing;
        use openrigcue_model::native_value::LegacyValue;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        {
            let mut controls = h.context.state.controls.write();
            controls.tests.motion_test = true;
            controls.tests.iflag_test = true;
            controls.tests.haptic_test = 3;
            controls.tests.belt_test = true;
            controls.switches.belt = true;
            controls.telemetry.capture = true;
            controls.telemetry.export = true;
            controls.switches.wind = crate::runtime::OutputMode::AlwaysOn;
            controls.switches.wind_center = crate::runtime::OutputMode::Off;
        }
        let mut belt = h.context.accessories.belt.lock().unwrap();
        belt.set_homing(Homing::InProgress);
        let main_loop = h.context.main_loop.lock().unwrap();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send([
                backend.control_value(&["tests", "motion_test"]),
                backend.control_value(&["tests", "iflag_test"]),
                backend.control_value(&["tests", "haptic_test"]),
                backend.control_value(&["tests", "belt_test"]),
                backend.control_value(&["switches", "belt"]),
                backend.control_value(&["tests", "belt_homing"]),
                backend.control_value(&["telemetry", "capture"]),
                backend.control_value(&["telemetry", "export"]),
                backend.control_value(&["switches", "wind"]),
                backend.control_value(&["switches", "wind_center"]),
            ])
            .unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(main_loop);
        drop(belt);
        worker.join().unwrap();
        let values = result.expect("parameter reads must not wait for MainLoop or serial I/O").map(Result::unwrap);
        assert_eq!(
            values,
            [
                LegacyValue::Int(1),
                LegacyValue::Int(1),
                LegacyValue::Int(3),
                LegacyValue::Int(1),
                LegacyValue::Int(1),
                LegacyValue::Int(3),
                LegacyValue::Bool(true),
                LegacyValue::Bool(true),
                LegacyValue::Int(2),
                LegacyValue::Int(0)
            ]
        );
    }

    /// Edits the stored settings as they are kept (native values).
    fn f5_edit_settings(
        h: &super::super::tests::Harness,
        edit: impl FnOnce(&mut openrigcue_model::native_value::Dict),
    ) {
        h.context.state.config.write().update(|configuration| configuration.settings.edit_native(edit));
    }

    /// The index of the settings group of a page that holds `control`.
    fn f5_section(page: &openrigcue_ui::spec::Page, control: &str) -> usize {
        page.sections.iter().position(|section| section.controls.iter().any(|c| c.id == control)).unwrap()
    }

    /// The tuning screens can be edited without a running game (the `allow_edit_emulator` custom feature), and
    /// the app is mounted on `screen`.
    fn f5_editable_app(
        h: &super::super::tests::Harness,
        screen: openrigcue_ui::ui::Screen,
    ) -> (Arc<DaemonBackend>, openrigcue_ui::app::MyApp) {
        f5_edit_settings(h, |raw| {
            raw.get_mut("CUSTOM_FEATURES")
                .unwrap()
                .assign_item("allow_edit_emulator", LegacyValue::Bool(true))
                .unwrap();
        });
        let backend = harness_backend(h);
        let app = mounted_app(backend.clone(), screen);
        (backend, app)
    }

    /// The loaded tune's value at `path`.
    fn f5_tune(h: &super::super::tests::Harness, path: &[&str]) -> LegacyValue {
        path.iter().fold(h.context.state.profile.read().native_tune(), |value, key| value.item_owned(key).unwrap())
    }

    #[test]
    fn hardware_sim3d_ports_and_invert_assignment_preserve_kinds_and_parents() {
        use openrigcue_effects::accessories::link::SerialLink;
        use openrigcue_effects::accessories::sim3d::{ShakeIt, Sim3d};
        use openrigcue_effects::accessories::status::Connection;
        use openrigcue_model::native_value::LegacyValue;
        struct SinkLink;
        impl SerialLink for SinkLink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len())
            }
            fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
                Ok(None)
            }
            fn read_available(&mut self) -> std::io::Result<Vec<u8>> {
                Ok(Vec::new())
            }
            fn bytes_waiting(&mut self) -> std::io::Result<usize> {
                Ok(0)
            }
            fn try_clone(&self) -> std::io::Result<Box<dyn SerialLink>> {
                Ok(Box::new(SinkLink))
            }
        }
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Hardware);
        h.context
            .accessories
            .sim3d
            .lock()
            .unwrap()
            .set_connection(Connection::Connected(Sim3d::new(ShakeIt::new(Box::new(SinkLink)))));
        let sim3d = f5_section(&openrigcue_ui::spec::hardware_page(), "rumble_port1");
        openrigcue_ui::ui::dispatch(&mut app, "section", "", &sim3d.to_string()).unwrap();
        let test_button = "data-arg=\"Hardware:rumble_port1:0\"";
        for (model, shown) in [("3 Ports", true), ("None", false)] {
            f5_edit_settings(&h, |raw| {
                raw.get_mut("SIM3D").unwrap().assign_item("model", LegacyValue::Str(model.into())).unwrap();
            });
            let html = openrigcue_ui::ui::screens::main(&app);
            assert_eq!(html.contains(test_button), shown, "{model}: the port rows follow the stored model");
            if shown {
                assert!(
                    html.contains(&format!("{test_button}>")),
                    "{model}: the connected SIM3D's port test is enabled with no game or test running"
                );
            }
        }
        // A model value that is not one of the model names (bytes spelling None) has no port rows.
        f5_edit_settings(&h, |raw| {
            raw.get_mut("SIM3D").unwrap().assign_item("model", LegacyValue::Bytes(b"None".to_vec())).unwrap();
        });
        assert!(!openrigcue_ui::ui::screens::main(&app).contains(test_button));
        assert_eq!(backend.native_setting("SIM3D", "model").unwrap(), LegacyValue::Bytes(b"None".to_vec()));

        // Reverse Traction Loss stores an integer flag in PROSIMU.
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:invert_tl", "1").unwrap();
        assert_eq!(backend.native_setting("PROSIMU", "invert_tl").unwrap(), LegacyValue::Int(1));
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:invert_tl", "0").unwrap();
        assert_eq!(backend.native_setting("PROSIMU", "invert_tl").unwrap(), LegacyValue::Int(0));
        // A section that is no dictionary refuses the write and stays as it is.
        f5_edit_settings(&h, |raw| {
            raw.insert("PROSIMU", LegacyValue::None);
        });
        let error = openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:invert_tl", "1").unwrap_err();
        assert!(error.starts_with("TypeError:"), "{error}");
        assert_eq!(h.context.state.config.read().settings().native_snapshot().get("PROSIMU"), Some(&LegacyValue::None));
        // A missing section names it.
        f5_edit_settings(&h, |raw| {
            raw.remove("PROSIMU");
        });
        assert_eq!(
            openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:invert_tl", "0").unwrap_err(),
            "KeyError: 'PROSIMU'"
        );
        assert!(h.context.state.config.read().settings().native_snapshot().get("PROSIMU").is_none());
        drop(app.take_threads());
    }

    #[test]
    fn hardware_scalar_values_store_integers_and_require_their_sections() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Hardware);
        for (row, section, key) in [
            ("brightness", "LED", "brightness"),
            ("iflag_dim_perc", "IFLAG", "brightness"),
            ("belt_calibration_power_slider", "BELT_TENSIONER", "belt_calibration_factor"),
            ("belt_calibration_offset_slider", "BELT_TENSIONER", "belt_calibration_retraction"),
        ] {
            let arg = format!("Hardware:{row}");
            openrigcue_ui::ui::dispatch(&mut app, "set", &arg, "23").unwrap();
            assert_eq!(backend.native_setting(section, key).unwrap(), LegacyValue::Int(23), "{row}");
            // Only a whole number as typed is stored: a fraction is refused and the stored value stays.
            let error = openrigcue_ui::ui::dispatch(&mut app, "set", &arg, "22.6").unwrap_err();
            assert!(error.starts_with("ValueError:"), "{row}: {error}");
            assert_eq!(backend.native_setting(section, key).unwrap(), LegacyValue::Int(23), "{row}: unchanged");
            let saved = h.context.state.config.read().settings().native_snapshot().get(section).unwrap().clone();
            h.context.state.config.write().update(|configuration| {
                configuration.settings.edit_native(|raw| {
                    raw.insert(section, LegacyValue::None);
                })
            });
            let error = openrigcue_ui::ui::dispatch(&mut app, "set", &arg, "31").unwrap_err();
            assert!(error.starts_with("TypeError:"), "{row}: {error}");
            assert_eq!(
                h.context.state.config.read().settings().native_snapshot().get(section),
                Some(&LegacyValue::None)
            );
            h.context.state.config.write().update(|configuration| {
                configuration.settings.edit_native(|raw| {
                    raw.insert(section, saved);
                })
            });
        }
        drop(app.take_threads());
    }

    #[test]
    fn soundcard_refresh_parameter_does_not_wait_for_main_loop_or_audio_io() {
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        h.context.shakers.lock().request_soundcard_refresh();
        let audio = h.context.shakers.lock();
        let main_loop = h.context.main_loop.lock().unwrap();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let before = backend.soundcard_refresh_requested();
            let clear = backend.call(Method::Delete, "shakers/refresh-soundcard", None);
            let after = backend.soundcard_refresh_requested();
            send.send((before, clear, after)).unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(main_loop);
        drop(audio);
        worker.join().unwrap();
        let (before, clear, after) = result.expect("the parameter and its acknowledgement must not wait for audio I/O");
        assert!(before.unwrap());
        clear.unwrap();
        assert!(!after.unwrap());
        assert!(!h.context.shakers.lock().status().refresh_soundcard);
    }

    #[test]
    fn wind_actual_power_readouts_follow_each_stored_value_without_the_main_loop() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Wind);
        let page = openrigcue_ui::ui::screens::wind::page();
        let (left, right) = (f5_section(&page, "fan_actual_perc"), f5_section(&page, "fan_center_actual_perc"));
        // The readouts follow the loaded tune as the main loop reads it again, also while the main loop's
        // service lock is held.
        let readouts = |mut app: openrigcue_ui::app::MyApp| {
            let main_loop = h.context.main_loop.lock().unwrap();
            let (send, receive) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                app.refresh_docs();
                let mut shown = Vec::new();
                for section in [left, right] {
                    app.ui.section = section;
                    shown.push(openrigcue_ui::ui::screens::main(&app));
                }
                send.send(shown).unwrap();
                app
            });
            let shown = receive.recv_timeout(std::time::Duration::from_secs(2));
            drop(main_loop);
            let app = worker.join().unwrap();
            (app, shown.expect("the readouts must not wait for the main loop"))
        };
        backend.assign_tune_value(&["power_wind_max_speed_perc"], &LegacyValue::Int(23)).unwrap();
        backend.assign_tune_value(&["power_center_wind_max_speed_perc"], &LegacyValue::Float(41.9)).unwrap();
        let (app, shown) = readouts(app);
        assert!(shown[0].contains(">23%<"), "the left / right actual power: {}", shown[0]);
        assert!(shown[1].contains(">41%<"), "the centre actual power, truncated: {}", shown[1]);
        // Each readout stands on its own value: a value that is no number leaves the other one shown.
        backend.assign_tune_value(&["power_wind_max_speed_perc"], &LegacyValue::Float(f64::INFINITY)).unwrap();
        backend.assign_tune_value(&["power_center_wind_max_speed_perc"], &LegacyValue::Bool(true)).unwrap();
        let (mut app, shown) = readouts(app);
        assert!(shown[1].contains(">1%<"), "a true value reads as 1: {}", shown[1]);
        assert!(!shown[0].contains(">23%<"), "the left readout follows its own value");
        assert!(
            matches!(f5_tune(&h, &["power_wind_max_speed_perc"]), LegacyValue::Float(value) if value.is_infinite())
        );
        assert_eq!(f5_tune(&h, &["power_center_wind_max_speed_perc"]), LegacyValue::Bool(true));
        drop(app.take_threads());
    }

    #[test]
    fn wizard_sample_accessor_retains_captured_nan_without_json_conversion() {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_sources::derive::{Dispatcher, Listener, Rate, RuntimeInputs, ScanHit, TickInput};
        struct NativeNan;
        impl Listener for NativeNan {
            fn id(&self) -> &str {
                "api"
            }
            fn is_running(&mut self) -> bool {
                true
            }
            fn receive(&mut self) {}
            fn channel(&self, _: &str) -> Option<f64> {
                Some(0.0)
            }
            fn native_channel(&self, name: &str) -> Option<LegacyValue> {
                Some(if name == "pitch" { LegacyValue::Float(f64::NAN) } else { LegacyValue::Int(0) })
            }
            fn rate(&self, _: Rate) -> Option<f64> {
                None
            }
            fn vehicle(&self) -> Option<String> {
                None
            }
            fn location(&self) -> Option<String> {
                None
            }
        }
        let mut h = super::super::tests::harness();
        let settings = crate::telemetry::dispatch_settings(h.context.state.config.read().settings());
        let profile = crate::telemetry::profile_settings(
            openrigcue_config::tunes::FactoryTunes::builtin().get("emulator").unwrap(),
        );
        let mut dispatcher = Dispatcher::new(vec![Box::new(NativeNan)]);
        dispatcher.tick(&TickInput {
            hits: &[ScanHit { game_id: "dirtrally2.0".into(), listener_id: "api".into() }],
            settings: &settings,
            profile: &profile,
            runtime: &RuntimeInputs {
                intellitune_capture: true,
                plot_channel: Some("pitch".into()),
                ..Default::default()
            },
            games: &crate::telemetry::CatalogDirectory::default(),
        });
        assert!(!dispatcher.has_ended());
        h.context.telemetry = crate::telemetry::TelemetryHandle::new(dispatcher);
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let samples = backend.intellitune_samples().unwrap();
        let LegacyValue::List(pitch) = samples.item_owned("pitch").unwrap() else { panic!("native pitch list") };
        assert!(matches!(pitch[0], LegacyValue::Float(value) if value.is_nan()));
        let LegacyValue::List(roll) = samples.item_owned("roll").unwrap() else { panic!("native roll list") };
        assert_eq!(roll[0], LegacyValue::Int(0));
        backend.set_intellitune_capture(true).unwrap();
        assert!(h.context.state.controls.read().telemetry.intellitune_capture);
        backend.clear_intellitune_samples().unwrap();
        assert!(backend.intellitune_samples().unwrap().as_dict().unwrap().is_empty());
        backend.set_intellitune_capture(false).unwrap();
        assert!(!h.context.state.controls.read().telemetry.intellitune_capture);
        let Runtime::Own(runtime) = &backend.runtime else { unreachable!() };
        assert!(runtime.get().is_none(), "sample reset/capture writes never create an API runtime");
        let main_loop = h.context.main_loop.lock().unwrap();
        let dispatcher = h.context.telemetry.lock();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || send.send(backend.telemetry_history()).unwrap());
        let history = receive.recv_timeout(std::time::Duration::from_secs(2));
        drop(main_loop);
        drop(dispatcher);
        worker.join().unwrap();
        let LegacyValue::List(history) = history.expect("history must not wait for listener I/O").unwrap() else {
            panic!("native history list")
        };
        assert_eq!(history.len(), 1);
        assert!(matches!(history[0], LegacyValue::Float(value) if value.is_nan()));
    }

    #[test]
    fn motion_parameters_read_driver_state_without_waiting_for_the_main_loop() {
        use openrigcue_devices::motion::LinkStatus;
        use openrigcue_model::native_value::LegacyValue;
        use std::sync::mpsc;
        use std::time::Duration;
        let h = super::super::tests::harness();
        let secondary = openrigcue_devices::motion::thanos::protocol::parse_config(
            openrigcue_devices::motion::thanos::protocol::ConfigKind::Long,
            &vec!["19"; 128].join(":"),
        )
        .unwrap();
        h.context
            .motion
            .driver_settings()
            .thanos_shared
            .update(|shared| shared.controller.secondary_config = secondary);
        let controller_state = h.context.motion.driver_settings().thanos_shared.get().controller;
        let controller = controller_state.controller;
        let firmware = controller_state.firmware;
        // The driver's shared link is still disconnected; the published status-view mirror is stale.
        h.context.state.motion.write().link.status = LinkStatus::Connected;
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let guard = h.context.main_loop.lock().unwrap();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send((
                backend.motion_parameter("motion_status"),
                backend.motion_parameter("thanos_config"),
                backend.motion_parameter("yaw_limit_enabled"),
                backend.motion_parameter("thanos_controller"),
                backend.motion_parameter("motion_com_port"),
                backend.motion_parameter("thanos_firmware"),
                backend.motion_parameter("thanos_config2"),
            ))
            .unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        drop(guard);
        worker.join().unwrap();
        let (status, thanos, yaw, current_controller, ports, current_firmware, secondary) =
            result.expect("driver parameters must not wait for the MainLoop lock");
        assert_eq!(status.unwrap(), LegacyValue::Str("Not connected".into()));
        assert_eq!(thanos.unwrap(), LegacyValue::Dict(Default::default()));
        assert_eq!(yaw.unwrap_err().detail, "KeyError: 'yaw_limit_enabled'");
        assert_eq!(current_controller.unwrap(), LegacyValue::Str(controller));
        assert_eq!(ports.unwrap(), LegacyValue::Dict(Default::default()));
        assert_eq!(current_firmware.unwrap(), LegacyValue::Str(firmware));
        assert_eq!(secondary.unwrap().item_owned("stroke1").unwrap(), LegacyValue::Int(19));
    }

    #[test]
    fn motion_smoothing_step_and_driving_style_use_native_settings_and_keep_errors() {
        use openrigcue_config::catalog::registry::Registry;
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let initial = detached(&h.context.state.profile.read().native_tune());
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Motion);
        // The step is read from the stored custom feature at each render.
        f5_edit_settings(&h, |raw| {
            raw.get_mut("CUSTOM_FEATURES")
                .unwrap()
                .assign_item("smoothing_slider_step", LegacyValue::Bytes(b"2".to_vec()))
                .unwrap();
        });
        let section = f5_section(&openrigcue_ui::spec::motion_page(Registry::builtin(), 1.0), "motion_smoothing_surge");
        openrigcue_ui::ui::dispatch(&mut app, "section", "", &section.to_string()).unwrap();
        let smoothing = |app: &openrigcue_ui::app::MyApp| {
            let html = openrigcue_ui::ui::screens::main(app);
            let at = html.find("data-arg=\"Motion:motion_smoothing_surge\"").expect("the surge smoothing row");
            let start = html[..at].rfind("<input").unwrap();
            html[start..at].to_owned()
        };
        assert!(smoothing(&app).contains("step=\"2\""), "the custom smoothing step: {}", smoothing(&app));
        // The driving style is stored as its name, with the effects and smoothing of the style (the row's write:
        // in stand-by, and without a stored style, the screen offers no choice).
        let write = openrigcue_ui::ui::screens::motion::write;
        write(&mut app, "intellitune", "", "0").unwrap();
        assert_eq!(f5_tune(&h, &["intellitune_motion_preference"]), LegacyValue::Str("Soft".into()));
        assert_eq!(f5_tune(&h, &["motion_effects", "yaw"]), LegacyValue::Int(0));
        assert!(matches!(f5_tune(&h, &["motion_effect_smoothing_factor", "surge"]), LegacyValue::Int(_)));
        // A loaded tune that is no dictionary refuses the write and stays as it is.
        h.context.state.profile.write().replace_native_tune(LegacyValue::Tuple(vec![]));
        let error = write(&mut app, "intellitune", "", "0").unwrap_err();
        assert!(error.starts_with("TypeError:"), "{error}");
        assert!(matches!(h.context.state.profile.read().native_tune().resolved(), LegacyValue::Tuple(_)));
        h.context.state.profile.write().replace_native_tune(initial);
        // A smoothing step that is no whole number: the rows step by 1.
        f5_edit_settings(&h, |raw| {
            raw.get_mut("CUSTOM_FEATURES")
                .unwrap()
                .assign_item("smoothing_slider_step", LegacyValue::Str("1.5".into()))
                .unwrap();
        });
        assert_eq!(openrigcue_ui::ui::screens::motion::smoothing_step(&app), 1.0);
        assert!(smoothing(&app).contains("step=\"1\""), "{}", smoothing(&app));
        assert_eq!(
            backend.native_setting("CUSTOM_FEATURES", "smoothing_slider_step").unwrap(),
            LegacyValue::Str("1.5".into())
        );
        drop(app.take_threads());
    }

    #[test]
    fn motion_platform_values_and_config_assignments_preserve_native_kinds_and_failure_prefixes() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Motion);
        let initial = detached(&h.context.state.profile.read().native_tune());
        // Stored values of other kinds are shown without being written back.
        for value in
            [LegacyValue::Bytes(b"37.0".to_vec()), LegacyValue::Float(f64::INFINITY), LegacyValue::Tuple(vec![])]
        {
            let mut raw = initial.clone();
            raw.assign_item("motion_max_power", value.clone()).unwrap();
            h.context.state.profile.write().replace_native_tune(raw);
            app.refresh_docs();
            assert!(openrigcue_ui::ui::screens::main(&app).contains("data-arg=\"Motion:motion_max_power\""));
            assert_eq!(f5_tune(&h, &["motion_max_power"]), value);
        }
        h.context.state.profile.write().replace_native_tune(initial);
        // The rows' writes (the app is in stand-by, where the screen takes no changes).
        let write = openrigcue_ui::ui::screens::motion::write;
        // Max Power keeps a whole number as an integer and a fraction as a float.
        write(&mut app, "motion_max_power", "", "37").unwrap();
        assert_eq!(f5_tune(&h, &["motion_max_power"]), LegacyValue::Int(37));
        assert!(openrigcue_ui::ui::screens::main(&app).contains(">37</output>"));
        write(&mut app, "motion_max_power", "", "37.5").unwrap();
        assert_eq!(f5_tune(&h, &["motion_max_power"]), LegacyValue::Float(37.5));
        write(&mut app, "gear_smoothing_factor", "", "37").unwrap();
        assert_eq!(f5_tune(&h, &["gear_smoothing_factor"]), LegacyValue::Int(37));
        // Refresh Rate is an integer motion setting.
        write(&mut app, "refresh_rate", "", "120").unwrap();
        assert_eq!(backend.native_setting("MOTIONBOX", "refresh_rate").unwrap(), LegacyValue::Int(120));
        // Reaction Speed: the tune value first, then the motion setting, whose failing section stays as it is.
        f5_edit_settings(&h, |raw| {
            raw.insert("MOTIONBOX", LegacyValue::None);
        });
        let error = write(&mut app, "reaction_speed", "", "85").unwrap_err();
        assert!(error.starts_with("TypeError:"), "{error}");
        assert_eq!(f5_tune(&h, &["motion_reaction_speed"]), LegacyValue::Int(85));
        assert_eq!(
            h.context.state.config.read().settings().native_snapshot().get("MOTIONBOX"),
            Some(&LegacyValue::None)
        );
        drop(app.take_threads());
    }

    #[test]
    fn motion_effect_values_switches_and_maxima_keep_kinds_and_refuse_bad_tables() {
        use openrigcue_config::catalog::registry::Registry;
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let (_backend, mut app) = f5_editable_app(&h, openrigcue_ui::ui::Screen::Motion);
        let initial = detached(&h.context.state.profile.read().native_tune());
        let key = Registry::builtin().motion_effects[0].id.as_str();
        let section = f5_section(&openrigcue_ui::spec::motion_page(Registry::builtin(), 1.0), key);
        openrigcue_ui::ui::dispatch(&mut app, "section", "", &section.to_string()).unwrap();
        let arg = format!("Motion:{key}");
        // Stored values of other kinds are shown without being written back.
        for value in [
            LegacyValue::Bytes(b"7.0".to_vec()),
            LegacyValue::Float(f64::NAN),
            LegacyValue::Float(f64::INFINITY),
            LegacyValue::Tuple(vec![]),
        ] {
            let mut raw = initial.clone();
            raw.as_dict_mut().unwrap().get_mut("motion_effects").unwrap().assign_item(key, value.clone()).unwrap();
            h.context.state.profile.write().replace_native_tune(raw);
            app.refresh_docs();
            assert!(openrigcue_ui::ui::screens::main(&app).contains(&format!("data-arg=\"{arg}\"")));
            let stored = f5_tune(&h, &["motion_effects", key]);
            assert!(
                openrigcue_model::python_compare::native_equal(&stored, &value)
                    || stored.py_float().is_some_and(f64::is_nan)
            );
        }
        h.context.state.profile.write().replace_native_tune(initial.clone());
        app.refresh_docs();
        // The effect keeps a whole number as an integer; a value that is no number is refused and changes nothing.
        openrigcue_ui::ui::dispatch(&mut app, "set", &arg, "7").unwrap();
        assert_eq!(f5_tune(&h, &["motion_effects", key]), LegacyValue::Int(7));
        assert!(openrigcue_ui::ui::dispatch(&mut app, "set", &arg, "nan").unwrap_err().starts_with("ValueError:"));
        assert_eq!(f5_tune(&h, &["motion_effects", key]), LegacyValue::Int(7));
        // The effect's switch is a boolean; off disables the row.
        openrigcue_ui::ui::dispatch(&mut app, "set", &format!("{arg}:toggle"), "0").unwrap();
        assert_eq!(f5_tune(&h, &["motion_switch", key]), LegacyValue::Bool(false));
        let disabled = format!("control-row is-disabled\" data-control=\"{key}\"");
        assert!(openrigcue_ui::ui::screens::main(&app).contains(&disabled), "the switched-off effect's row");
        openrigcue_ui::ui::dispatch(&mut app, "set", &format!("{arg}:toggle"), "1").unwrap();
        assert_eq!(f5_tune(&h, &["motion_switch", key]), LegacyValue::Bool(true));
        assert!(!openrigcue_ui::ui::screens::main(&app).contains(&disabled));
        // Max Telemetry is an integer; a table that is no dictionary refuses the write and stays as it is.
        openrigcue_ui::ui::dispatch(&mut app, "set", &format!("{arg}:max"), "8").unwrap();
        assert_eq!(f5_tune(&h, &["max_telemetry_vars", key]), LegacyValue::Int(8));
        let mut raw = initial;
        raw.assign_item("max_telemetry_vars", LegacyValue::Tuple(vec![])).unwrap();
        h.context.state.profile.write().replace_native_tune(raw);
        assert!(openrigcue_ui::ui::dispatch(&mut app, "set", &format!("{arg}:max"), "9").is_err());
        assert!(matches!(f5_tune(&h, &["max_telemetry_vars"]).resolved(), LegacyValue::Tuple(_)));
        drop(app.take_threads());
    }

    #[test]
    fn motion_smoothing_actuator_and_spike_filter_writes_keep_integers_and_failures() {
        use openrigcue_config::catalog::registry::Registry;
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let (_backend, mut app) = f5_editable_app(&h, openrigcue_ui::ui::Screen::Motion);
        let initial = detached(&h.context.state.profile.read().native_tune());
        // The smoothing and vertical-actuator rows store whole numbers as integers; a value that is no number is
        // refused and changes nothing.
        for (row, path) in [
            ("motion_smoothing_surge", vec!["motion_effect_smoothing_factor", "surge"]),
            ("motion_main_actuators_surge", vec!["motion_main_actuators_surge"]),
        ] {
            // The rows' write: the platform here has no vertical actuators, whose rows the screen does not offer.
            let write = openrigcue_ui::ui::screens::motion::write;
            write(&mut app, row, "", "7").unwrap();
            assert_eq!(f5_tune(&h, &path), LegacyValue::Int(7), "{row}");
            assert!(write(&mut app, row, "", "nan").unwrap_err().starts_with("ValueError:"));
            assert_eq!(f5_tune(&h, &path), LegacyValue::Int(7), "{row}");
        }
        // A smoothing table that is no dictionary refuses the write and stays as it is.
        let mut raw = initial.clone();
        raw.assign_item("motion_effect_smoothing_factor", LegacyValue::Tuple(vec![])).unwrap();
        h.context.state.profile.write().replace_native_tune(raw);
        assert!(openrigcue_ui::ui::dispatch(&mut app, "set", "Motion:motion_smoothing_surge", "8").is_err());
        assert!(matches!(f5_tune(&h, &["motion_effect_smoothing_factor"]).resolved(), LegacyValue::Tuple(_)));
        h.context.state.profile.write().replace_native_tune(initial);
        app.refresh_docs();

        // The spike filters of a Thanos platform: integers, shown in millimetres of the vertical stroke.
        let model = openrigcue_ui::catalog::models().iter().find(|model| model.manufacturer == "THANOS").unwrap();
        f5_edit_settings(&h, |raw| {
            raw.get_mut("MOTIONBOX").unwrap().assign_item("model", LegacyValue::Str(model.name.clone())).unwrap();
            raw.get_mut("THANOS").unwrap().assign_item("vertical_actuators_stroke", LegacyValue::Int(100)).unwrap();
        });
        // The row's write: the screen takes the filters once the controller reports a configuration.
        openrigcue_ui::ui::screens::motion::write(&mut app, "thanos_spike_filter_horizontal", "", "6553").unwrap();
        assert_eq!(f5_tune(&h, &["thanos_spike_filter_horizontal"]), LegacyValue::Int(6553));
        let section =
            f5_section(&openrigcue_ui::spec::motion_page(Registry::builtin(), 1.0), "thanos_spike_filter_horizontal");
        openrigcue_ui::ui::dispatch(&mut app, "section", "", &section.to_string()).unwrap();
        let html = openrigcue_ui::ui::screens::main(&app);
        assert!(
            html.contains("data-arg=\"Motion:thanos_spike_filter_horizontal\""),
            "a Thanos platform's spike filter"
        );
        assert!(html.contains(">10.00 mm<"), "6553 / 65535 × 100 mm: {html}");
        // A stored spike filter that is no number is shown without being written back.
        let mut raw = detached(&h.context.state.profile.read().native_tune());
        raw.assign_item("thanos_spike_filter_vertical", LegacyValue::Float(f64::NAN)).unwrap();
        h.context.state.profile.write().replace_native_tune(raw);
        app.refresh_docs();
        assert!(openrigcue_ui::ui::screens::main(&app).contains("data-arg=\"Motion:thanos_spike_filter_vertical\""));
        let vertical = f5_tune(&h, &["thanos_spike_filter_vertical"]);
        assert!(matches!(vertical.resolved(), LegacyValue::Float(value) if value.is_nan()), "{vertical:?}");
        drop(app.take_threads());
    }

    #[test]
    fn effect_config_gates_read_native_values_before_touching_effects() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        h.context.state.config.write().update(|configuration| configuration.settings.app.enable_hapticr = true);
        let (backend, mut app) = f5_editable_app(&h, openrigcue_ui::ui::Screen::HapticR);
        assert_eq!(app.ui.screen, openrigcue_ui::ui::Screen::HapticR);
        let initial = detached(&h.context.state.profile.read().native_tune());
        let key = openrigcue_config::catalog::registry::Registry::builtin().haptic_effects[0].id.as_str();
        // The stored flag is read as it is kept, not from the settings document: neither false nor a value that
        // is no boolean offers HapticR or lets its profile-load defaults touch the effects.
        for value in [LegacyValue::Bool(false), LegacyValue::Float(1.0)] {
            let mut raw = initial.clone();
            raw.assign_item("haptic_effects", LegacyValue::Tuple(vec![])).unwrap();
            h.context.state.profile.write().replace_native_tune(raw);
            f5_edit_settings(&h, |raw| {
                raw.get_mut("APP").unwrap().assign_item("enable_hapticr", value.clone()).unwrap();
            });
            app.refresh_docs();
            app.docs.settings["app"]["enable_hapticr"] = Value::Bool(true);
            assert_eq!(backend.native_setting("APP", "enable_hapticr").unwrap(), value);
            assert!(!openrigcue_ui::ui::screens::hapticr::enabled(&app), "{value:?}");
            // A flag that is no boolean word stops the profile load's rows at HapticR's.
            let loaded = openrigcue_ui::ui::screens::reload::profile_load(&mut app);
            assert_eq!(loaded.is_err(), value != LegacyValue::Bool(false), "{value:?} {loaded:?}");
            openrigcue_ui::ui::navigate(&mut app, openrigcue_ui::ui::Screen::HapticR, 0);
            assert_ne!(app.ui.screen, openrigcue_ui::ui::Screen::HapticR, "{value:?}");
            assert_eq!(f5_tune(&h, &["haptic_effects"]), LegacyValue::Tuple(vec![]));
        }
        // A boolean word turns HapticR on, whatever the settings document says; the screen shows the stored
        // effect, and the other tuning screens render with the custom feature kept as bytes.
        f5_edit_settings(&h, |raw| {
            raw.get_mut("APP").unwrap().assign_item("enable_hapticr", LegacyValue::Str("y".into())).unwrap();
            raw.get_mut("CUSTOM_FEATURES")
                .unwrap()
                .assign_item("hide_effects_not_in_game", LegacyValue::Bytes(b"yes".to_vec()))
                .unwrap();
        });
        assert_eq!(
            backend.native_setting("CUSTOM_FEATURES", "hide_effects_not_in_game").unwrap(),
            LegacyValue::Bytes(b"yes".to_vec())
        );
        let mut raw = initial;
        raw.as_dict_mut().unwrap().get_mut("haptic_effects").unwrap().assign_item(key, LegacyValue::Int(7)).unwrap();
        h.context.state.profile.write().replace_native_tune(raw);
        app.refresh_docs();
        app.docs.settings["app"]["enable_hapticr"] = Value::Bool(false);
        assert!(openrigcue_ui::ui::screens::hapticr::enabled(&app));
        let section = f5_section(&openrigcue_ui::ui::screens::hapticr::page(), key);
        openrigcue_ui::ui::navigate(&mut app, openrigcue_ui::ui::Screen::HapticR, section);
        assert_eq!(app.ui.screen, openrigcue_ui::ui::Screen::HapticR);
        let html = openrigcue_ui::ui::screens::main(&app);
        let at = html.find(&format!("data-arg=\"HapticR:{key}\"")).expect("the stored effect's row");
        assert!(html[html[..at].rfind("<input").unwrap()..at].contains("value=\"7\""));
        openrigcue_ui::ui::screens::reload::profile_load(&mut app).unwrap();
        assert_eq!(f5_tune(&h, &["haptic_effects", key]), LegacyValue::Int(7), "a stored effect keeps its value");
        for screen in
            [openrigcue_ui::ui::Screen::Sim3d, openrigcue_ui::ui::Screen::Shake, openrigcue_ui::ui::Screen::Motion]
        {
            openrigcue_ui::ui::navigate(&mut app, screen, 0);
            assert_eq!(app.ui.screen, screen);
            assert!(!openrigcue_ui::ui::screens::main(&app).is_empty(), "{screen:?}");
        }
        drop(app.take_threads());
    }

    #[test]
    fn wind_native_config_reads_and_range_writes_preserve_source_values_and_errors() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let (_backend, mut app) = f5_editable_app(&h, openrigcue_ui::ui::Screen::Wind);
        let page = openrigcue_ui::ui::screens::wind::page();
        let settings = h.context.state.config.read().settings().clone();
        let initial = detached(&h.context.state.profile.read().native_tune());
        // The fan count is compared as it is kept: bytes spelling 2 are not the text "2", so wind curving is
        // off with its effect at 1, and a loaded profile takes those values.
        f5_edit_settings(&h, |raw| {
            raw.get_mut("INTELLIBOX").unwrap().assign_item("nbr_fans", LegacyValue::Bytes(b"2".to_vec())).unwrap();
        });
        let mut raw = initial.clone();
        raw.assign_item("wind_curving", LegacyValue::Int(1)).unwrap();
        raw.assign_item("curving_effect", LegacyValue::Int(6)).unwrap();
        h.context.state.profile.write().replace_native_tune(raw);
        app.refresh_docs();
        app.docs.settings["wind"]["fan_count"] = Value::from("2");
        assert!(!openrigcue_ui::ui::screens::wind::two_fans(&app));
        openrigcue_ui::ui::dispatch(&mut app, "section", "", &f5_section(&page, "curving_effect").to_string()).unwrap();
        let html = openrigcue_ui::ui::screens::main(&app);
        assert!(html.contains("control-row is-disabled\" data-control=\"curving_effect\""), "{html}");
        assert!(html.contains("Needs 2 fans (Hardware)"));
        openrigcue_ui::ui::screens::reload::profile_load(&mut app).unwrap();
        assert_eq!(f5_tune(&h, &["wind_curving"]), LegacyValue::Int(0));
        assert_eq!(f5_tune(&h, &["curving_effect"]), LegacyValue::Int(1));
        // With the text "2" the rows are offered.
        f5_edit_settings(&h, |raw| {
            raw.get_mut("INTELLIBOX").unwrap().assign_item("nbr_fans", LegacyValue::Str("2".into())).unwrap();
        });
        assert!(openrigcue_ui::ui::screens::wind::two_fans(&app));
        assert!(!openrigcue_ui::ui::screens::main(&app).contains("is-disabled\" data-control=\"curving_effect\""));

        // Both power ranges store their ends as integers.
        for (row, prefix) in [("fan_power_perc", "power_wind"), ("fan_power_center_perc", "power_center_wind")] {
            openrigcue_ui::ui::dispatch(&mut app, "set", &format!("Wind:{row}:high"), "76").unwrap();
            openrigcue_ui::ui::dispatch(&mut app, "set", &format!("Wind:{row}"), "23").unwrap();
            assert_eq!(f5_tune(&h, &[&format!("{prefix}_max_speed_perc")]), LegacyValue::Int(76), "{row}");
            assert_eq!(f5_tune(&h, &[&format!("{prefix}_min_speed_perc")]), LegacyValue::Int(23), "{row}");
            openrigcue_ui::ui::dispatch(&mut app, "section", "", &f5_section(&page, row).to_string()).unwrap();
            assert!(openrigcue_ui::ui::screens::main(&app).contains("23% - 76%"), "{row}");
        }

        // Without the App section (or its unit) the speed is shown and stored in km/h; nothing is written by
        // showing it.
        h.context.state.profile.write().replace_native_tune(initial);
        h.context.state.config.write().update(|configuration| {
            configuration.settings = settings.clone();
            configuration.settings.edit_native(|raw| {
                raw.get_mut("CUSTOM_FEATURES")
                    .unwrap()
                    .assign_item("allow_edit_emulator", LegacyValue::Bool(true))
                    .unwrap();
                raw.insert("APP", LegacyValue::None);
            });
        });
        app.refresh_docs();
        let speed = f5_tune(&h, &["default_max_speed_kmh"]);
        assert!(openrigcue_ui::ui::screens::wind::kmh(&app));
        openrigcue_ui::ui::dispatch(&mut app, "section", "", &f5_section(&page, "default_max_speed").to_string())
            .unwrap();
        assert!(openrigcue_ui::ui::screens::main(&app).contains("Km/h"));
        assert_eq!(f5_tune(&h, &["default_max_speed_kmh"]), speed);
        openrigcue_ui::ui::dispatch(&mut app, "set", "Wind:default_max_speed", "250").unwrap();
        assert_eq!(f5_tune(&h, &["default_max_speed_kmh"]), LegacyValue::Int(250));
        h.context.state.config.write().update(|configuration| {
            configuration.settings = settings;
            configuration.settings.edit_native(|raw| {
                raw.get_mut("CUSTOM_FEATURES")
                    .unwrap()
                    .assign_item("allow_edit_emulator", LegacyValue::Bool(true))
                    .unwrap();
                raw.get_mut("APP").unwrap().as_dict_mut().unwrap().remove("measurement_unit");
            });
        });
        assert!(openrigcue_ui::ui::screens::wind::kmh(&app));
        openrigcue_ui::ui::dispatch(&mut app, "set", "Wind:default_max_speed", "240").unwrap();
        assert_eq!(f5_tune(&h, &["default_max_speed_kmh"]), LegacyValue::Int(240));
        drop(app.take_threads());
    }

    #[test]
    fn native_hardware_wind_choices_preserve_current_values_and_failure_prefixes() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Hardware);
        let page = openrigcue_ui::spec::hardware_page();
        let option = |control: &str, label: &str| {
            let Some(openrigcue_ui::spec::Control { widget: openrigcue_ui::spec::Widget::Select { options }, .. }) =
                page.control(control)
            else {
                panic!("{control} is a list")
            };
            options.iter().position(|choice| choice.label == label).unwrap().to_string()
        };
        let mut tune = detached(&h.context.state.profile.read().native_tune());
        tune.assign_item("speed_factor", LegacyValue::Bytes(b"9".to_vec())).unwrap();
        tune.assign_item("wind_curving", LegacyValue::Bytes(b"yes".to_vec())).unwrap();
        h.context.state.profile.write().replace_native_tune(tune);
        // `2+` stores the text "2"; two fans leave the wind values of the profile as they are.
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:nbr_fans", &option("nbr_fans", "2+")).unwrap();
        assert_eq!(backend.native_setting("INTELLIBOX", "nbr_fans").unwrap(), LegacyValue::Str("2".into()));
        assert_eq!(f5_tune(&h, &["wind_curving"]), LegacyValue::Bytes(b"yes".to_vec()));
        assert_eq!(f5_tune(&h, &["speed_factor"]), LegacyValue::Bytes(b"9".to_vec()));
        // One fan: wind curving off, its effect 1.
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:nbr_fans", &option("nbr_fans", "1")).unwrap();
        assert_eq!(backend.native_setting("INTELLIBOX", "nbr_fans").unwrap(), LegacyValue::Str("1".into()));
        assert_eq!(f5_tune(&h, &["wind_curving"]), LegacyValue::Int(0));
        assert_eq!(f5_tune(&h, &["curving_effect"]), LegacyValue::Int(1));
        // The wind type: its code, and the power range it allows.
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:wind_type", &option("wind_type", "Fan")).unwrap();
        assert_eq!(backend.native_setting("INTELLIBOX", "wind_type").unwrap(), LegacyValue::Str("F".into()));
        assert_eq!(f5_tune(&h, &["power_wind_max_speed_perc"]), LegacyValue::Float(100.0));
        assert_eq!(f5_tune(&h, &["power_wind_min_speed_perc"]), LegacyValue::Float(0.0));

        // A section that is no dictionary refuses both writes before any tune value changes.
        let before = detached(&h.context.state.profile.read().native_tune());
        f5_edit_settings(&h, |raw| {
            raw.insert("INTELLIBOX", LegacyValue::None);
        });
        let error =
            openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:nbr_fans", &option("nbr_fans", "1")).unwrap_err();
        assert!(error.starts_with("TypeError:"), "{error}");
        let error = openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:wind_type", &option("wind_type", "Fan"))
            .unwrap_err();
        assert!(error.starts_with("TypeError:"), "{error}");
        assert_eq!(
            h.context.state.config.read().settings().native_snapshot().get("INTELLIBOX"),
            Some(&LegacyValue::None)
        );
        assert!(openrigcue_model::python_compare::native_equal(&h.context.state.profile.read().native_tune(), &before));
        drop(app.take_threads());
    }

    /// f6: the row of `control` in a rendered screen body, up to the next row.
    fn f6_row(html: &str, control: &str) -> String {
        let marker = format!("data-control=\"{control}\"");
        let Some(at) = html.find(&marker) else { return String::new() };
        // From the row's opening tag (its class says whether the row is disabled).
        let start = html[..at].rfind("<div class=\"control-row").unwrap_or(at);
        let rest = &html[start..];
        let marker = &rest[..at - start + marker.len()];
        let end = rest[marker.len()..].find("<div class=\"control-row").map_or(rest.len(), |at| at + marker.len());
        rest[..end].to_owned()
    }

    /// f6: the value the browser sends for the entry `label` of a row's list (its index in the list).
    fn f6_option(row: &str, label: &str) -> String {
        let end = row.find(&format!(">{label}</option>")).unwrap_or_else(|| panic!("no option {label:?} in {row}"));
        let start = row[..end].rfind("<option value=\"").unwrap() + "<option value=\"".len();
        row[start..].split('"').next().unwrap().to_owned()
    }

    /// f6: a stored setting as the service keeps it.
    fn f6_setting(h: &super::super::tests::Harness, section: &str, key: &str) -> LegacyValue {
        h.context.state.config.read().settings().native_snapshot().get(section).unwrap().item_owned(key).unwrap()
    }

    /// f6: the Hardware screen's body at settings group `section`.
    fn f6_hardware(app: &mut openrigcue_ui::app::MyApp, section: usize) -> String {
        openrigcue_ui::ui::dispatch(app, "section", "", &section.to_string()).unwrap();
        openrigcue_ui::ui::screens::main(app)
    }

    #[test]
    fn hardware_device_rows_store_ports_and_switches_and_require_the_device_section() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Hardware);
        // Com Port: index 0 of the list is Auto Search (an empty text), index 9 is COM9.
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:com_port_BELT_TENSIONER", "9").unwrap();
        assert_eq!(f6_setting(&h, "BELT_TENSIONER", "com_port"), LegacyValue::Str("COM9".into()));
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:auto_search_BELT_TENSIONER", "0").unwrap();
        assert_eq!(f6_setting(&h, "BELT_TENSIONER", "auto_search"), LegacyValue::Bool(false));
        assert!(!f6_row(&f6_hardware(&mut app, 4), "auto_search_BELT_TENSIONER").contains(" checked"));
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:auto_search_BELT_TENSIONER", "1").unwrap();
        assert_eq!(f6_setting(&h, "BELT_TENSIONER", "auto_search"), LegacyValue::Bool(true));
        assert!(f6_row(&f6_hardware(&mut app, 4), "auto_search_BELT_TENSIONER").contains(" checked"));
        // The start-up work does not rewrite a device's port.
        openrigcue_ui::ui::screens::hardware::start(&mut app).unwrap();
        assert_eq!(f6_setting(&h, "BELT_TENSIONER", "com_port"), LegacyValue::Str("COM9".into()));
        // Without the device's section neither row writes; the failure reaches the action's caller.
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| raw.insert("BELT_TENSIONER", LegacyValue::None));
        });
        let error =
            openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:auto_search_BELT_TENSIONER", "0").unwrap_err();
        assert!(error.starts_with("TypeError:"), "{error}");
        let error = openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:com_port_BELT_TENSIONER", "7").unwrap_err();
        assert!(error.starts_with("TypeError:"), "{error}");
        assert_eq!(
            h.context.state.config.read().settings().native_snapshot().get("BELT_TENSIONER"),
            Some(&LegacyValue::None)
        );
        drop(app.take_threads());
    }

    #[test]
    fn hardware_led_count_keeps_segments_in_range_and_stops_at_a_bad_segment() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Hardware);
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                let led = raw.get_mut("LED").unwrap();
                for (key, value) in [
                    ("segment1_start", LegacyValue::Bytes(b"7".to_vec())),
                    ("segment1_end", LegacyValue::Bytes(b"11".to_vec())),
                    ("segment2_start", LegacyValue::Int(15)),
                    ("segment2_end", LegacyValue::Int(17)),
                    ("segment4_start", LegacyValue::Int(100)),
                    ("segment4_end", LegacyValue::Int(140)),
                    ("future_field", LegacyValue::Bytes(b"retain".to_vec())),
                ] {
                    led.assign_item(key, value).unwrap();
                }
            });
        });
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:nbr_leds", "120").unwrap();
        assert_eq!(f6_setting(&h, "LED", "nbr_leds"), LegacyValue::Int(120));
        // A segment end beyond the count is brought within it; bounds already within it are not written again.
        assert_eq!(f6_setting(&h, "LED", "segment4_start"), LegacyValue::Int(100));
        assert_eq!(f6_setting(&h, "LED", "segment4_end"), LegacyValue::Int(120));
        assert_eq!(f6_setting(&h, "LED", "segment1_start"), LegacyValue::Bytes(b"7".to_vec()));
        assert_eq!(f6_setting(&h, "LED", "segment1_end"), LegacyValue::Bytes(b"11".to_vec()));
        assert_eq!(f6_setting(&h, "LED", "segment2_start"), LegacyValue::Int(15));
        assert_eq!(f6_setting(&h, "LED", "segment2_end"), LegacyValue::Int(17));
        assert_eq!(f6_setting(&h, "LED", "future_field"), LegacyValue::Bytes(b"retain".to_vec()));
        // A segment without a readable end stops the change before the later segments and the count.
        h.context.state.config.write().update(|configuration| {
            configuration
                .settings
                .edit_native(|raw| raw.get_mut("LED").unwrap().assign_item("segment3_end", LegacyValue::None).unwrap());
        });
        let error = openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:nbr_leds", "110").unwrap_err();
        assert!(error.starts_with("TypeError:"), "{error}");
        assert_eq!(f6_setting(&h, "LED", "nbr_leds"), LegacyValue::Int(120));
        assert_eq!(f6_setting(&h, "LED", "segment3_end"), LegacyValue::None);
        assert_eq!(f6_setting(&h, "LED", "segment4_end"), LegacyValue::Int(120), "the fourth segment is not reached");
        // The start-up work reads the stored count and stops at the same segment, keeping the stored count.
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("LED").unwrap().assign_item("nbr_leds", LegacyValue::Bytes(b"115".to_vec())).unwrap();
            });
        });
        let error = openrigcue_ui::ui::screens::hardware::start(&mut app).unwrap_err();
        assert!(error.starts_with("TypeError:"), "{error}");
        assert_eq!(f6_setting(&h, "LED", "nbr_leds"), LegacyValue::Bytes(b"115".to_vec()));
        assert_eq!(f6_setting(&h, "LED", "segment4_end"), LegacyValue::Int(120));
        drop(app.take_threads());
    }

    #[test]
    fn hardware_led_model_callback_reloads_the_named_color_order() {
        use openrigcue_effects::accessories::led::LedByteOrder;
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Hardware);
        h.context.accessories.load_hex_colors("RGB");
        let row = f6_row(&f6_hardware(&mut app, 0), "led_model");
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:led_model", &f6_option(&row, "GRB")).unwrap();
        assert_eq!(f6_setting(&h, "LED", "model"), LegacyValue::Str("GRB".into()));
        assert_eq!(h.context.accessories.hex_colors(), Some(LedByteOrder::Grb));
        assert!(f6_row(&f6_hardware(&mut app, 0), "led_model").contains("selected>GRB</option>"));
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("LED").unwrap().assign_item("model", LegacyValue::Str("RGB".into())).unwrap()
            });
        });
        assert_eq!(
            h.context.accessories.hex_colors(),
            Some(LedByteOrder::Grb),
            "other settings assignments do not reload the named colors"
        );
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:led_model", &f6_option(&row, "RGB")).unwrap();
        assert_eq!(h.context.accessories.hex_colors(), Some(LedByteOrder::Rgb));
        // A stored model that is none of the model names reloads no named order.
        let bytes = LegacyValue::Bytes(b"GRB".to_vec());
        backend.assign_setting_value("LED", "model", &bytes).unwrap();
        backend.reload_led_colors().unwrap();
        assert_eq!(f6_setting(&h, "LED", "model"), bytes);
        assert_eq!(h.context.accessories.hex_colors(), None, "bytes differ from the model names");
        drop(app.take_threads());
    }

    #[test]
    fn hardware_iflag_and_led_rows_show_and_store_their_kinds() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Hardware);
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                let iflag = raw.get_mut("IFLAG").unwrap();
                iflag.assign_item("brightness", LegacyValue::Int(47)).unwrap();
                iflag.assign_item("rotation", LegacyValue::Int(270)).unwrap();
                iflag.assign_item("mirror", LegacyValue::Bool(true)).unwrap();
                raw.get_mut("LED").unwrap().assign_item("brightness", LegacyValue::Int(61)).unwrap();
            });
        });
        app.refresh_docs();
        let iflag = f6_hardware(&mut app, 7);
        assert!(f6_row(&iflag, "iflag_dim_perc").contains("value=\"47\""), "{iflag}");
        assert!(f6_row(&iflag, "iflag_orientation").contains("selected>Right</option>"), "{iflag}");
        assert!(f6_row(&iflag, "iflag_mirror").contains(" checked"), "{iflag}");
        let intellibox = f6_hardware(&mut app, 0);
        assert!(f6_row(&intellibox, "brightness").contains("value=\"61\""), "{intellibox}");
        // The rows store whole numbers as integers and the mirror as a boolean.
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:iflag_dim_perc", "33").unwrap();
        assert_eq!(f6_setting(&h, "IFLAG", "brightness"), LegacyValue::Int(33));
        let left = f6_option(&f6_row(&iflag, "iflag_orientation"), "Left");
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:iflag_orientation", &left).unwrap();
        assert_eq!(f6_setting(&h, "IFLAG", "rotation"), LegacyValue::Int(90));
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:iflag_mirror", "0").unwrap();
        assert_eq!(f6_setting(&h, "IFLAG", "mirror"), LegacyValue::Bool(false));
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:brightness", "19").unwrap();
        assert_eq!(f6_setting(&h, "LED", "brightness"), LegacyValue::Int(19));
        // Without the iFlag section the rows fail and the LED brightness stays.
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| raw.insert("IFLAG", LegacyValue::None));
        });
        let error = openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:iflag_dim_perc", "20").unwrap_err();
        assert!(error.starts_with("TypeError:"), "{error}");
        assert_eq!(f6_setting(&h, "LED", "brightness"), LegacyValue::Int(19));
        drop(app.take_threads());
    }

    #[test]
    fn hardware_smoothing_and_belt_calibration_rows_show_and_store_their_kinds() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Hardware);
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("MOTIONBOX")
                    .unwrap()
                    .assign_item("auto_smoothing_low_speed", LegacyValue::Str("yes".into()))
                    .unwrap();
                let belt = raw.get_mut("BELT_TENSIONER").unwrap();
                belt.assign_item("belt_calibration_factor", LegacyValue::Int(43)).unwrap();
                belt.assign_item("belt_calibration_retraction", LegacyValue::Int(57)).unwrap();
            });
        });
        app.refresh_docs();
        let motion = f6_hardware(&mut app, 2);
        assert!(f6_row(&motion, "auto_smoothing_low_speed").contains(" checked"), "a stored \"yes\" shows on");
        let belt = f6_hardware(&mut app, 4);
        assert!(f6_row(&belt, "belt_calibration_power_slider").contains("value=\"43\""), "{belt}");
        assert!(f6_row(&belt, "belt_calibration_offset_slider").contains("value=\"57\""), "{belt}");
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:auto_smoothing_low_speed", "1").unwrap();
        assert_eq!(f6_setting(&h, "MOTIONBOX", "auto_smoothing_low_speed"), LegacyValue::Int(1));
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:auto_smoothing_low_speed", "0").unwrap();
        assert_eq!(f6_setting(&h, "MOTIONBOX", "auto_smoothing_low_speed"), LegacyValue::Int(0));
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:belt_calibration_power_slider", "44").unwrap();
        assert_eq!(f6_setting(&h, "BELT_TENSIONER", "belt_calibration_factor"), LegacyValue::Int(44));
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:belt_calibration_offset_slider", "58").unwrap();
        assert_eq!(f6_setting(&h, "BELT_TENSIONER", "belt_calibration_retraction"), LegacyValue::Int(58));
        drop(app.take_threads());
    }

    #[test]
    fn native_shaker_profile_callback_loads_despite_an_unrelated_projection_failure() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Hardware);
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("LED").unwrap().assign_item("future_field", LegacyValue::Bytes(b"retain".to_vec())).unwrap()
            });
        });
        app.docs.settings["shakers"]["sound_card"] = serde_json::json!("Stale Card");
        let row = f6_row(&f6_hardware(&mut app, 1), "shakers_profile");
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:shakers_profile", &f6_option(&row, "SRS UShake6"))
            .unwrap();
        assert_eq!(h.context.state.shaker_layout.read().name, "SRS UShake6");
        assert_eq!(f6_setting(&h, "SHAKEBOX", "profile"), LegacyValue::Str("SRS UShake6".into()));
        assert_eq!(f6_setting(&h, "LED", "future_field"), LegacyValue::Bytes(b"retain".to_vec()));
        app.docs.settings["shakers"]["sound_card"] = serde_json::json!("Stale Card");
        let shakers = f6_hardware(&mut app, 1);
        assert!(f6_row(&shakers, "shakers_profile").contains("selected>SRS UShake6</option>"), "{shakers}");
        assert!(
            f6_row(&shakers, "fl").contains("is-disabled"),
            "the speaker row follows the stored sound device (None), not the stale cache: {shakers}"
        );
        drop(app.take_threads());
    }

    #[test]
    fn native_sim3d_model_reads_current_ports_without_projection_or_coercion() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Hardware);
        openrigcue_ui::ui::screens::hardware::start(&mut app).unwrap();
        let first = LegacyValue::Bytes(b"Throttle".to_vec());
        let fourth = LegacyValue::Tuple(vec![LegacyValue::Int(17)]);
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                let sim3d = raw.get_mut("SIM3D").unwrap();
                sim3d.assign_item("port1", first.clone()).unwrap();
                sim3d.assign_item("port4", fourth.clone()).unwrap();
            });
        });
        let sim3d = f6_hardware(&mut app, 5);
        assert!(!f6_row(&sim3d, "rumble_port4").contains("data-control"), "a port beyond the model's is not shown");
        let model = f6_option(&f6_row(&sim3d, "rumble_model"), "4 Ports");
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:rumble_model", &model).unwrap();
        assert_eq!(f6_setting(&h, "SIM3D", "port1"), first);
        assert_eq!(f6_setting(&h, "SIM3D", "port4"), fourth);
        assert_eq!(f6_setting(&h, "SIM3D", "model"), LegacyValue::Str("4 Ports".into()));
        // Which ports are offered follows the stored model; the shown values come from the settings document,
        // which cannot be read while a port holds a value it cannot represent.
        let sim3d = f6_hardware(&mut app, 5);
        assert!(f6_row(&sim3d, "rumble_port4").contains("data-control"), "the fourth port is offered: {sim3d}");
        assert!(openrigcue_ui::backend::BackendExt::get(&*app.backend, "settings").is_err());
        drop(app.take_threads());
    }

    #[test]
    fn native_motion_axis_lookup_requires_the_original_field_after_model_assignment() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Hardware);
        let h3 = openrigcue_ui::catalog::model("H3").unwrap();
        let picker = f6_row(&f6_hardware(&mut app, 2), "motion_manufacturer");
        openrigcue_ui::ui::dispatch(
            &mut app,
            "set",
            "Hardware:motion_manufacturer",
            &f6_option(&picker, &h3.manufacturer),
        )
        .unwrap();
        let models = f6_row(&f6_hardware(&mut app, 2), "motion_model");
        let choice = f6_option(&models, "H3");
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap().remove("axis_assignment");
            });
        });
        let error = openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:motion_model", &choice).unwrap_err();
        assert_eq!(error, "KeyError: 'axis_assignment'");
        assert_eq!(f6_setting(&h, "MOTIONBOX", "model"), LegacyValue::Str("H3".into()), "stored before the lookup");
        assert!(h
            .context
            .state
            .config
            .read()
            .settings()
            .native_snapshot()
            .get("MOTIONBOX")
            .unwrap()
            .item_owned("axis_assignment")
            .is_err());
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("MOTIONBOX")
                    .unwrap()
                    .assign_item("axis_assignment", LegacyValue::Bytes(b"native".to_vec()))
                    .unwrap();
            });
        });
        // The model's rules read the setting without converting or validating it.
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:motion_model", &choice).unwrap();
        assert_eq!(f6_setting(&h, "MOTIONBOX", "axis_assignment"), LegacyValue::Bytes(b"native".to_vec()));
        drop(app.take_threads());
    }

    #[test]
    fn native_motion_startup_uses_the_current_model_despite_unrelated_projection_failure() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Hardware);
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.get_mut("MOTIONBOX").unwrap().assign_item("model", LegacyValue::Str("H3".into())).unwrap();
                raw.get_mut("LED")
                    .unwrap()
                    .assign_item("future_field", LegacyValue::Bytes(b"retain".to_vec()))
                    .unwrap();
            });
        });
        openrigcue_ui::ui::screens::hardware::start(&mut app).unwrap();
        assert_eq!(f6_setting(&h, "MOTIONBOX", "model"), LegacyValue::Str("H3".into()));
        assert_eq!(f6_setting(&h, "LED", "future_field"), LegacyValue::Bytes(b"retain".to_vec()));
        let motion = f6_hardware(&mut app, 2);
        assert!(f6_row(&motion, "motion_model").contains("selected>H3</option>"), "{motion}");
        let manufacturer = &openrigcue_ui::catalog::model("H3").unwrap().manufacturer;
        assert!(
            f6_row(&motion, "motion_manufacturer").contains(&format!("selected>{manufacturer}</option>")),
            "{motion}"
        );
        drop(app.take_threads());
    }

    #[test]
    fn native_motion_controls_read_native_profile_fields_and_preserve_manual_port_values() {
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), openrigcue_ui::ui::Screen::Hardware);
        let model = openrigcue_ui::catalog::models()
            .iter()
            .find(|model| model.name.contains("2 TL") && !model.manual_com_port)
            .unwrap();
        let picker = f6_row(&f6_hardware(&mut app, 2), "motion_manufacturer");
        openrigcue_ui::ui::dispatch(
            &mut app,
            "set",
            "Hardware:motion_manufacturer",
            &f6_option(&picker, &model.manufacturer),
        )
        .unwrap();
        let mut tune = h.context.state.profile.read().native_tune();
        tune.assign_item("min_traction_loss_front_activation", LegacyValue::Bytes(b"17".to_vec())).unwrap();
        tune.assign_item("motion_main_actuators_traction_loss_front", LegacyValue::Bytes(b"9".to_vec())).unwrap();
        tune.assign_item("future_field", LegacyValue::Bytes(b"retain".to_vec())).unwrap();
        h.context.state.profile.write().replace_native_tune(tune);
        let models = f6_row(&f6_hardware(&mut app, 2), "motion_model");
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:motion_model", &f6_option(&models, &model.name))
            .unwrap();
        assert_eq!(f6_setting(&h, "MOTIONBOX", "model"), LegacyValue::Str(model.name.clone()));
        // A "2 TL" model puts the front activation threshold into its number box again (stored as the whole number
        // the box takes), keeps the vertical-actuator share as stored and turns the effect on.
        let tune = h.context.state.profile.read().native_tune();
        assert_eq!(tune.item_owned("min_traction_loss_front_activation").unwrap(), LegacyValue::Int(17));
        assert_eq!(
            tune.item_owned("motion_main_actuators_traction_loss_front").unwrap(),
            LegacyValue::Bytes(b"9".to_vec())
        );
        assert_eq!(tune.item_owned("future_field").unwrap(), LegacyValue::Bytes(b"retain".to_vec()));
        assert_eq!(
            tune.item_owned("motion_switch").unwrap().item_owned("traction_loss_front").unwrap(),
            LegacyValue::Bool(true)
        );
        // The manual ports store the chosen entry (index 0 is the empty entry) in the manufacturer's section.
        openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:motion_port", "0").unwrap();
        assert_eq!(f6_setting(&h, "PROSIMU", "prs200_com_port1"), LegacyValue::Str(String::new()));
        h.context.state.config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| raw.insert("PROSIMU", LegacyValue::None));
        });
        let error = openrigcue_ui::ui::dispatch(&mut app, "set", "Hardware:motion_port2", "0").unwrap_err();
        assert!(error.starts_with("TypeError:"), "{error}");
        drop(app.take_threads());
    }

    /// Lifts the stand-by lock of the tuning screens (`allow_edit_emulator` of the custom features), so their
    /// writes reach the service without a running game.
    fn f7_unlock(backend: &DaemonBackend) {
        backend.assign_setting_value("CUSTOM_FEATURES", "allow_edit_emulator", &LegacyValue::Bool(true)).unwrap();
    }

    /// Opens `screen` at the first settings group that shows the element `id` and returns its body.
    fn f7_html_with(app: &mut openrigcue_ui::app::MyApp, screen: openrigcue_ui::ui::Screen, id: &str) -> String {
        let needle = format!("id=\"{id}\"");
        for section in 0..16 {
            openrigcue_ui::ui::navigate(app, screen, section);
            let html = openrigcue_ui::ui::screens::main(app);
            if html.contains(&needle) {
                return html;
            }
        }
        panic!("no element {id} on {screen:?}");
    }

    /// The opening tag of the element `id` in `html`.
    fn f7_tag<'a>(html: &'a str, id: &str) -> &'a str {
        let at = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("no element {id}"));
        let start = html[..at].rfind('<').unwrap();
        &html[start..at + html[at..].find('>').unwrap()]
    }

    /// The attribute `name` of the element `id` in `html`.
    fn f7_attr(html: &str, id: &str, name: &str) -> Option<String> {
        let tag = f7_tag(html, id);
        let at = tag.find(&format!(" {name}=\""))? + name.len() + 3;
        Some(tag[at..at + tag[at..].find('"')?].to_owned())
    }

    /// Whether the checkbox `id` in `html` is checked.
    fn f7_checked(html: &str, id: &str) -> bool {
        f7_tag(html, id).contains(" checked")
    }

    /// The stored native value at `path` of the loaded tune.
    fn f7_tune(h: &super::super::tests::Harness, path: &[&str]) -> LegacyValue {
        let mut value = h.context.state.profile.read().native_tune();
        for key in path {
            value = value.item_owned(key).unwrap();
        }
        value
    }

    /// Installs `initial` as the loaded tune with `value` at `path` (one or two keys deep).
    fn f7_with(h: &super::super::tests::Harness, initial: &LegacyValue, path: &[&str], value: LegacyValue) {
        let mut raw = initial.clone();
        match path {
            [key] => {
                raw.as_dict_mut().unwrap().insert(*key, value);
            }
            [table, key] => raw.as_dict_mut().unwrap().get_mut(table).unwrap().assign_item(key, value).unwrap(),
            _ => unreachable!("one or two keys"),
        }
        h.context.state.profile.write().replace_native_tune(raw);
    }

    /// Equal values of the same kind (a float compared by its bits, so NaN equals NaN).
    fn f7_same(a: &LegacyValue, b: &LegacyValue) -> bool {
        match (a.resolved(), b.resolved()) {
            (LegacyValue::Float(x), LegacyValue::Float(y)) => x.to_bits() == y.to_bits(),
            (x, y) => x == y,
        }
    }

    /// Stored values that are no plain number, as a profile file can hold them.
    fn f7_malformed() -> Vec<LegacyValue> {
        vec![
            LegacyValue::Float(f64::NAN),
            LegacyValue::Float(f64::INFINITY),
            LegacyValue::Float(f64::NEG_INFINITY),
            LegacyValue::Bytes(b"37.0".to_vec()),
            LegacyValue::Tuple(vec![]),
            LegacyValue::None,
        ]
    }

    /// Tuning → LED's shift point and Hardware's shift light dimming: a whole number from the browser is stored
    /// as an integer and a fraction as a float; a text that is no finite number is refused before any write.
    /// The slider shows the stored value once the documents are read again (a number given as text included),
    /// a stored value that is no plain number shows the slider's minimum, and reading never writes the stored
    /// value back.
    #[test]
    fn shift_point_and_dimming_writes_keep_number_kinds_and_reads_leave_the_native_value() {
        use openrigcue_ui::ui::{dispatch as ui_act, Screen};
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), Screen::Led);
        let initial = detached(&h.context.state.profile.read().native_tune());
        for (screen, arg, field, id) in [
            (Screen::Led, "LED:gear_shift_perc", "gear_shift_perc", "c-led-gear_shift_perc"),
            (
                Screen::Hardware,
                "Hardware:shift_light_dim_perc",
                "gear_shift_dim_perc",
                "c-hardware-shift_light_dim_perc",
            ),
        ] {
            h.context.state.profile.write().replace_native_tune(initial.clone());
            app.refresh_docs();
            f7_html_with(&mut app, screen, id);
            ui_act(&mut app, "set", arg, "37").unwrap();
            assert_eq!(f7_tune(&h, &[field]), LegacyValue::Int(37));
            let html = openrigcue_ui::ui::screens::main(&app);
            assert_eq!(f7_attr(&html, id, "value").as_deref(), Some("37"), "{arg}");
            ui_act(&mut app, "set", arg, "37.5").unwrap();
            assert_eq!(f7_tune(&h, &[field]), LegacyValue::Float(37.5));
            for refused in ["NaN", "inf", "", "x"] {
                let error = ui_act(&mut app, "set", arg, refused).unwrap_err();
                assert!(error.starts_with("ValueError:"), "{refused}: {error}");
                assert_eq!(f7_tune(&h, &[field]), LegacyValue::Float(37.5), "{refused}");
            }
            let mut stored_values = vec![
                (LegacyValue::Int(63), "63"),
                (LegacyValue::Float(42.5), "42.5"),
                (LegacyValue::Str("37".into()), "37"),
                (LegacyValue::Bool(true), "1"),
            ];
            stored_values.extend(f7_malformed().into_iter().map(|value| (value, "0")));
            for (stored, shown) in stored_values {
                f7_with(&h, &initial, &[field], stored.clone());
                app.refresh_docs();
                let html = openrigcue_ui::ui::screens::main(&app);
                assert_eq!(f7_attr(&html, id, "value").as_deref(), Some(shown), "{arg} {stored:?}");
                assert!(f7_same(&f7_tune(&h, &[field]), &stored), "reading does not write {stored:?} back");
            }
        }
        drop(app.take_threads());
    }

    /// Tuning → SIM3D and HapticR: an effect slider stores a whole number (rounded), the advanced RPM start
    /// keeps a fraction, a text that is no finite number is refused before any write, the slider shows the
    /// stored value (the minimum for a value that is no plain number, which stays stored as it was), and a
    /// write into a table the loaded tune holds as a tuple fails with the service's error and changes nothing.
    #[test]
    fn sim3d_and_hapticr_writes_keep_number_kinds_and_stop_at_a_bad_parent() {
        use openrigcue_ui::ui::{components::control_id, dispatch as ui_act, Screen};
        let h = super::super::tests::harness();
        h.context.state.config.write().update(|configuration| configuration.settings.app.enable_hapticr = true);
        let backend = harness_backend(&h);
        f7_unlock(&backend);
        let mut app = mounted_app(backend.clone(), Screen::Sim3d);
        let initial = detached(&h.context.state.profile.read().native_tune());
        let registry = openrigcue_config::catalog::registry::Registry::builtin();
        for (screen, table, key, rpm_row) in [
            (Screen::Sim3d, "rumble_effects", registry.rumble_effects[0].id.as_str(), "rpm_start_rumble_perc"),
            (Screen::HapticR, "haptic_effects", registry.haptic_effects[0].id.as_str(), "rpm_start_haptic_perc"),
        ] {
            let prefix = screen.key();
            h.context.state.profile.write().replace_native_tune(initial.clone());
            app.refresh_docs();
            let id = control_id(prefix, key, "");
            f7_html_with(&mut app, screen, &id);
            let arg = format!("{prefix}:{key}");
            ui_act(&mut app, "set", &arg, "7").unwrap();
            assert_eq!(f7_tune(&h, &[table, key]), LegacyValue::Int(7), "{prefix}");
            ui_act(&mut app, "set", &arg, "7.6").unwrap();
            assert_eq!(f7_tune(&h, &[table, key]), LegacyValue::Int(8), "{prefix}");
            let error = ui_act(&mut app, "set", &arg, "NaN").unwrap_err();
            assert!(error.starts_with("ValueError:"), "{error}");
            assert_eq!(f7_tune(&h, &[table, key]), LegacyValue::Int(8));
            let html = openrigcue_ui::ui::screens::main(&app);
            assert_eq!(f7_attr(&html, &id, "value").as_deref(), Some("8"), "{prefix}");
            for stored in f7_malformed() {
                f7_with(&h, &initial, &[table, key], stored.clone());
                app.refresh_docs();
                let html = openrigcue_ui::ui::screens::main(&app);
                assert_eq!(f7_attr(&html, &id, "value").as_deref(), Some("0"), "{prefix} {stored:?}");
                assert!(f7_same(&f7_tune(&h, &[table, key]), &stored), "{prefix}: {stored:?} stays stored");
            }

            h.context.state.profile.write().replace_native_tune(initial.clone());
            app.refresh_docs();
            let rpm_id = control_id(prefix, rpm_row, "");
            f7_html_with(&mut app, screen, &rpm_id);
            let rpm_arg = format!("{prefix}:{rpm_row}");
            ui_act(&mut app, "set", &rpm_arg, "7").unwrap();
            assert_eq!(f7_tune(&h, &[table, "rpm_start_rumble"]), LegacyValue::Int(7), "{prefix}");
            ui_act(&mut app, "set", &rpm_arg, "7.5").unwrap();
            assert_eq!(f7_tune(&h, &[table, "rpm_start_rumble"]), LegacyValue::Float(7.5), "{prefix}");
            let error = ui_act(&mut app, "set", &rpm_arg, "inf").unwrap_err();
            assert!(error.starts_with("ValueError:"), "{error}");
            assert_eq!(f7_tune(&h, &[table, "rpm_start_rumble"]), LegacyValue::Float(7.5));

            f7_with(&h, &initial, &[table], LegacyValue::Tuple(vec![]));
            app.refresh_docs();
            for (arg, value) in [(&rpm_arg, "8"), (&arg, "8")] {
                let error = ui_act(&mut app, "set", arg, value).unwrap_err();
                assert_eq!(error, "TypeError: 'tuple' object does not support item assignment", "{arg}");
                assert_eq!(f7_tune(&h, &[table]), LegacyValue::Tuple(vec![]), "{arg}");
            }
        }
        h.context.state.profile.write().replace_native_tune(initial);
        let error = backend.assign_tune_value(&["missing_native_parent", "value"], &LegacyValue::Int(8)).unwrap_err();
        assert_eq!(error.detail, "KeyError: 'missing_native_parent'");
        drop(app.take_threads());
    }

    /// Tuning → SIM3D and HapticR's RPM fade range: each end is stored as a whole number and kept on its side
    /// of the other end; the range shows the stored ends; a value that is no plain number shows the minimum and
    /// stays stored. On a loaded profile HapticR gives a missing fade end its default (94 – 100) and leaves a
    /// present one alone.
    #[test]
    fn sim3d_and_hapticr_fade_range_writes_keep_whole_ends_in_order() {
        use openrigcue_ui::ui::{components::control_id, dispatch as ui_act, Screen};
        let h = super::super::tests::harness();
        h.context.state.config.write().update(|configuration| configuration.settings.app.enable_hapticr = true);
        let backend = harness_backend(&h);
        f7_unlock(&backend);
        let mut app = mounted_app(backend, Screen::Sim3d);
        let initial = detached(&h.context.state.profile.read().native_tune());
        for (screen, table) in [(Screen::Sim3d, "rumble_effects"), (Screen::HapticR, "haptic_effects")] {
            let prefix = screen.key();
            h.context.state.profile.write().replace_native_tune(initial.clone());
            app.refresh_docs();
            let id = control_id(prefix, "rpm_fading_perc", "");
            f7_html_with(&mut app, screen, &id);
            let low = format!("{prefix}:rpm_fading_perc");
            let high = format!("{prefix}:rpm_fading_perc:high");
            let ends = || (f7_tune(&h, &[table, "rpm_start_fading"]), f7_tune(&h, &[table, "rpm_stop_fading"]));
            ui_act(&mut app, "set", &low, "0").unwrap();
            ui_act(&mut app, "set", &high, "91").unwrap();
            ui_act(&mut app, "set", &low, "63.4").unwrap();
            assert_eq!(ends(), (LegacyValue::Int(63), LegacyValue::Int(91)), "{prefix}");
            ui_act(&mut app, "set", &low, "95").unwrap();
            assert_eq!(ends(), (LegacyValue::Int(91), LegacyValue::Int(91)), "the low end stays below the high end");
            ui_act(&mut app, "set", &low, "63").unwrap();
            ui_act(&mut app, "set", &high, "50").unwrap();
            assert_eq!(ends(), (LegacyValue::Int(63), LegacyValue::Int(63)), "the high end stays above the low end");
            ui_act(&mut app, "set", &high, "91").unwrap();
            let error = ui_act(&mut app, "set", &high, "NaN").unwrap_err();
            assert!(error.starts_with("ValueError:"), "{error}");
            assert_eq!(ends(), (LegacyValue::Int(63), LegacyValue::Int(91)));
            let html = openrigcue_ui::ui::screens::main(&app);
            assert_eq!(f7_attr(&html, &id, "value").as_deref(), Some("63"), "{prefix}");
            assert_eq!(f7_attr(&html, &format!("{id}-high"), "value").as_deref(), Some("91"), "{prefix}");
            for stored in f7_malformed() {
                f7_with(&h, &initial, &[table, "rpm_start_fading"], stored.clone());
                app.refresh_docs();
                let html = openrigcue_ui::ui::screens::main(&app);
                assert_eq!(f7_attr(&html, &id, "value").as_deref(), Some("0"), "{prefix} {stored:?}");
                assert!(f7_same(&f7_tune(&h, &[table, "rpm_start_fading"]), &stored), "{stored:?} stays stored");
            }
        }
        f7_with(&h, &initial, &["haptic_effects", "rpm_start_fading"], LegacyValue::None);
        let with_start = detached(&h.context.state.profile.read().native_tune());
        f7_with(&h, &with_start, &["haptic_effects", "rpm_stop_fading"], LegacyValue::Int(91));
        app.refresh_docs();
        openrigcue_ui::ui::screens::reload::profile_load(&mut app).unwrap();
        // Without a usable start the fade range takes 94 – 100, the stored stop included.
        assert_eq!(f7_tune(&h, &["haptic_effects", "rpm_start_fading"]), LegacyValue::Int(94));
        assert_eq!(f7_tune(&h, &["haptic_effects", "rpm_stop_fading"]), LegacyValue::Int(100));
        drop(app.take_threads());
    }

    #[test]
    fn shaker_callback_refreshes_groups_when_the_value_already_matches_the_profile() {
        use openrigcue_effects::shakers::Effect;
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        backend.assign_shaker_value("shaker_switch", "gear", &LegacyValue::Bool(true)).unwrap();
        backend.assign_shaker_value("shaker_effects", "gear", &LegacyValue::Int(4)).unwrap();
        let level = || h.context.shakers.lock().system.settings().tuning.effect(Effect::Gear).level.unwrap().slider;
        assert_eq!(level(), 4.0);
        {
            let mut profile = h.context.state.profile.write();
            profile.tune.shaker_effects.gear = 5.0;
            profile.generation += 1;
        }
        assert_eq!(level(), 4.0, "editing the profile alone has not refreshed the groups");
        backend.assign_tune_value(&["rumble_effects", "rpm_start_rumble"], &LegacyValue::Int(7)).unwrap();
        assert_eq!(level(), 4.0, "ordinary native tune assignments do not refresh shaker groups");
        backend.assign_shaker_value("shaker_effects", "gear", &LegacyValue::Int(5)).unwrap();
        assert_eq!(level(), 5.0, "the callback refreshes groups even when the assigned level is unchanged");
    }

    #[test]
    fn native_shaker_rebuild_failure_preserves_assignments_output_prefix_and_retry() {
        use openrigcue_effects::shakers::{Group, Speaker};
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let original = h.context.state.shaker_layout.read().value.clone();
        h.context
            .state
            .shaker_layout
            .write()
            .value
            .as_dict_mut()
            .unwrap()
            .get_mut("CE")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .get_mut("sfx")
            .unwrap()
            .assign_item("gear", LegacyValue::from("9"))
            .unwrap();
        let error = backend.assign_shaker_value("shaker_volume", "ce", &LegacyValue::Int(7)).unwrap_err();
        assert_eq!(error.detail, "TypeError: unsupported operand type(s) for /: 'str' and 'int'");
        assert_eq!(
            h.context.state.profile.read().native_tune().item_owned("shaker_volume").unwrap().item_owned("ce").unwrap(),
            LegacyValue::Int(7),
            "the stored slider assignment precedes rebuild failure"
        );
        {
            let shakers = h.context.shakers.lock();
            assert_eq!(shakers.system.settings().tuning.volume[Speaker::Center.index()], 7.0);
            assert!(shakers.system.levels().required_of(Group::Engine).is_ok());
            assert_eq!(shakers.system.levels().required_of(Group::Gear).unwrap_err().message, "'gear'");
        }
        h.context.state.shaker_layout.write().value = original;
        backend.assign_shaker_value("shaker_volume", "ce", &LegacyValue::Int(7)).unwrap();
        assert!(h.context.shakers.lock().system.levels().required_of(Group::Gear).is_ok());
    }

    #[test]
    fn native_shaker_confirmation_keeps_matrix_assignments_before_rebuild_error() {
        use openrigcue_effects::shakers::Group;
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        let mut levels = backend.shaker_effects().unwrap().item_owned("CE").unwrap().item_owned("sfx").unwrap().clone();
        levels.assign_item("gear", LegacyValue::from("9")).unwrap();
        let error = backend.confirm_shaker_levels(Some("CE"), &levels).unwrap_err();
        assert_eq!(error.detail, "TypeError: unsupported operand type(s) for /: 'str' and 'int'");
        assert_eq!(backend.shaker_effects().unwrap().item_owned("CE").unwrap().item_owned("sfx").unwrap(), levels);
        assert!(h.context.shakers.lock().system.levels().required_of(Group::Engine).is_ok());
        assert!(h.context.shakers.lock().system.levels().required_of(Group::Gear).is_err());
        levels.assign_item("gear", LegacyValue::Int(9)).unwrap();
        backend.confirm_shaker_levels(Some("CE"), &levels).unwrap();
        assert!(h.context.shakers.lock().system.levels().required_of(Group::Gear).is_ok());
    }

    /// Tuning → Shake's effect sliders and Hardware's channel volumes: a whole number is stored as an integer
    /// and a fraction as a float, a text that is no finite number is refused before any write, the slider shows
    /// the stored value (the minimum for one that is no plain number, which stays stored), a channel volume is
    /// stored even when another channel's tuple makes the shaker rebuild fail, and a write into an effects table the
    /// loaded tune holds as a tuple fails with the service's error and changes nothing.
    #[test]
    fn shake_and_channel_volume_writes_keep_number_kinds_and_stop_at_a_bad_parent() {
        use openrigcue_ui::ui::{components::control_id, dispatch as ui_act, Screen};
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        f7_unlock(&backend);
        let mut app = mounted_app(backend.clone(), Screen::Shake);
        let initial = detached(&h.context.state.profile.read().native_tune());
        let key = openrigcue_config::catalog::registry::Registry::builtin().shaker_effects[0].id.as_str();
        let id = control_id("Shake", key, "");
        f7_html_with(&mut app, Screen::Shake, &id);
        let arg = format!("Shake:{key}");
        ui_act(&mut app, "set", &arg, "7").unwrap();
        assert_eq!(f7_tune(&h, &["shaker_effects", key]), LegacyValue::Int(7));
        ui_act(&mut app, "set", &arg, "7.5").unwrap();
        assert_eq!(f7_tune(&h, &["shaker_effects", key]), LegacyValue::Float(7.5));
        for refused in ["NaN", "-inf", "seven"] {
            let error = ui_act(&mut app, "set", &arg, refused).unwrap_err();
            assert!(error.starts_with("ValueError:"), "{refused}: {error}");
            assert_eq!(f7_tune(&h, &["shaker_effects", key]), LegacyValue::Float(7.5), "{refused}");
        }
        let html = openrigcue_ui::ui::screens::main(&app);
        assert_eq!(f7_attr(&html, &id, "value").as_deref(), Some("7.5"));
        for stored in f7_malformed() {
            f7_with(&h, &initial, &["shaker_effects", key], stored.clone());
            app.refresh_docs();
            let html = openrigcue_ui::ui::screens::main(&app);
            assert_eq!(f7_attr(&html, &id, "value").as_deref(), Some("0"), "{stored:?}");
            assert!(f7_same(&f7_tune(&h, &["shaker_effects", key]), &stored), "{stored:?} stays stored");
        }

        h.context.state.profile.write().replace_native_tune(initial.clone());
        app.refresh_docs();
        f7_html_with(&mut app, Screen::Hardware, "c-hardware-rr");
        ui_act(&mut app, "set", "Hardware:rr", "6").unwrap();
        assert_eq!(f7_tune(&h, &["shaker_volume", "rr"]), LegacyValue::Int(6));
        ui_act(&mut app, "set", "Hardware:rr", "6.5").unwrap();
        assert_eq!(f7_tune(&h, &["shaker_volume", "rr"]), LegacyValue::Float(6.5));
        let error = ui_act(&mut app, "set", "Hardware:rr", "NaN").unwrap_err();
        assert!(error.starts_with("ValueError:"), "{error}");
        assert_eq!(f7_tune(&h, &["shaker_volume", "rr"]), LegacyValue::Float(6.5));
        let html = openrigcue_ui::ui::screens::main(&app);
        assert_eq!(f7_attr(&html, "c-hardware-rr", "value").as_deref(), Some("6.5"));
        // Another channel holding a tuple: the volume is stored, then the shaker rebuild reading every channel
        // fails with the service's error.
        f7_with(&h, &initial, &["shaker_volume", "rl"], LegacyValue::Tuple(vec![]));
        app.refresh_docs();
        assert_eq!(
            ui_act(&mut app, "set", "Hardware:rr", "4"),
            Err("TypeError: unsupported operand type(s) for /: 'tuple' and 'float'".into())
        );
        assert_eq!(f7_tune(&h, &["shaker_volume", "rr"]), LegacyValue::Int(4), "stored before the rebuild failed");
        assert_eq!(f7_tune(&h, &["shaker_volume", "rl"]), LegacyValue::Tuple(vec![]), "the other channel stays");

        f7_with(&h, &initial, &["shaker_effects"], LegacyValue::Tuple(vec![]));
        app.refresh_docs();
        openrigcue_ui::ui::navigate(&mut app, Screen::Shake, 0);
        let error = ui_act(&mut app, "set", &arg, "8").unwrap_err();
        assert_eq!(error, "TypeError: 'tuple' object does not support item assignment");
        assert_eq!(f7_tune(&h, &["shaker_effects"]), LegacyValue::Tuple(vec![]));
        let error = backend.assign_shaker_value("shaker_effects", key, &LegacyValue::Int(8)).unwrap_err();
        assert_eq!(error.detail, "TypeError: 'tuple' object does not support item assignment");
        drop(app.take_threads());
    }

    /// Tuning → Belt and IFlag write into the profile the service has loaded: when the loaded tune (or its
    /// `iflag` table) is `None`, the write fails with the service's error, which `dispatch` returns (the
    /// interface shows it as a warning), and the stored value stays `None`.
    #[test]
    fn belt_and_iflag_writes_require_the_current_profile_parent() {
        use openrigcue_config::profiles::ProfileStore;
        use openrigcue_model::native_value::Dict;
        use openrigcue_ui::ui::{components::control_id, dispatch as ui_act, Screen};
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        f7_unlock(&backend);
        let mut app = mounted_app(backend, Screen::Belt);
        // A present vehicle entry is returned unchanged by the profile resolver.
        let mut vehicles = Dict::new();
        vehicles.insert("car", LegacyValue::None);
        let mut game = Dict::new();
        game.insert("vehicles", LegacyValue::Dict(vehicles));
        let (native, _) = ProfileStore::resolve_native_row("api", Some("car"), &game.into()).unwrap();
        assert_eq!(native, LegacyValue::None);
        h.context.state.profile.write().replace_native_tune(native);
        app.refresh_docs();
        f7_html_with(&mut app, Screen::Belt, "c-belt-belt_surge");
        assert_eq!(
            ui_act(&mut app, "set", "Belt:belt_surge", "7"),
            Err("TypeError: 'NoneType' object does not support item assignment".into())
        );
        assert_eq!(h.context.state.profile.read().native_tune(), LegacyValue::None);
        // A normal profile whose nested IFlag table alone is `None`.
        let mut native = openrigcue_config::tunes::FactoryTunes::builtin().get("emulator").unwrap().to_json();
        native["iflag"] = Value::Null;
        h.context.state.profile.write().replace_native_tune(native.into());
        app.refresh_docs();
        let flag = openrigcue_config::catalog::registry::Registry::builtin()
            .flags
            .iter()
            .find(|flag| flag.group != "none")
            .unwrap();
        f7_html_with(&mut app, Screen::IFlag, &control_id("IFlag", &flag.name, ""));
        assert_eq!(
            ui_act(&mut app, "set", &format!("IFlag:{}", flag.name), "1"),
            Err("TypeError: 'NoneType' object does not support item assignment".into())
        );
        assert_eq!(f7_tune(&h, &["iflag"]), LegacyValue::None);
        drop(app.take_threads());
    }

    /// Tuning → Belt: sliders store whole numbers (rounded) and switches 1 / 0, a text that is no finite number
    /// is refused before any write; every row shows its stored value (a slider the minimum for a value that is
    /// no plain number, a switch off for a tuple), whatever another row holds, and reading writes nothing back.
    #[test]
    fn belt_writes_store_integers_and_reads_leave_the_native_values() {
        use openrigcue_ui::ui::{dispatch as ui_act, Screen};
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        f7_unlock(&backend);
        let mut app = mounted_app(backend, Screen::Belt);
        let initial = detached(&h.context.state.profile.read().native_tune());
        let curving = "c-belt-belt_curving";
        f7_html_with(&mut app, Screen::Belt, curving);
        ui_act(&mut app, "set", "Belt:belt_curving", "7").unwrap();
        assert_eq!(f7_tune(&h, &["belt_sway_curving_factor"]), LegacyValue::Int(7));
        ui_act(&mut app, "set", "Belt:belt_curving", "7.6").unwrap();
        assert_eq!(f7_tune(&h, &["belt_sway_curving_factor"]), LegacyValue::Int(8));
        let error = ui_act(&mut app, "set", "Belt:belt_curving", "NaN").unwrap_err();
        assert!(error.starts_with("ValueError:"), "{error}");
        assert_eq!(f7_tune(&h, &["belt_sway_curving_factor"]), LegacyValue::Int(8));
        assert_eq!(f7_attr(&openrigcue_ui::ui::screens::main(&app), curving, "value").as_deref(), Some("8"));
        let surge = "c-belt-belt_surge_enabled";
        f7_html_with(&mut app, Screen::Belt, surge);
        ui_act(&mut app, "set", "Belt:belt_surge_enabled", "1").unwrap();
        assert_eq!(f7_tune(&h, &["belt_surge_enabled"]), LegacyValue::Int(1));
        assert!(f7_checked(&openrigcue_ui::ui::screens::main(&app), surge));
        ui_act(&mut app, "set", "Belt:belt_surge_enabled", "0").unwrap();
        assert_eq!(f7_tune(&h, &["belt_surge_enabled"]), LegacyValue::Int(0));
        assert!(!f7_checked(&openrigcue_ui::ui::screens::main(&app), surge));
        for stored in f7_malformed() {
            let mut raw = initial.clone();
            let fields = raw.as_dict_mut().unwrap();
            fields.insert("belt_sway_curving_factor", stored.clone());
            fields.insert("belt_surge_enabled", LegacyValue::Tuple(vec![]));
            fields.insert("belt_surge", LegacyValue::Int(16));
            h.context.state.profile.write().replace_native_tune(raw);
            app.refresh_docs();
            let html = f7_html_with(&mut app, Screen::Belt, curving);
            assert_eq!(f7_attr(&html, curving, "value").as_deref(), Some("0"), "{stored:?}");
            let html = f7_html_with(&mut app, Screen::Belt, surge);
            assert!(!f7_checked(&html, surge));
            assert_eq!(f7_attr(&html, "c-belt-belt_surge", "value").as_deref(), Some("16"));
            assert!(f7_same(&f7_tune(&h, &["belt_sway_curving_factor"]), &stored), "{stored:?} stays stored");
            assert_eq!(f7_tune(&h, &["belt_surge_enabled"]), LegacyValue::Tuple(vec![]));
            assert_eq!(f7_tune(&h, &["belt_surge"]), LegacyValue::Int(16));
        }
        drop(app.take_threads());
    }

    /// Tuning → Wind: the default maximum speed is stored in km/h as a whole number, shown and entered in mph
    /// when the App settings measure in miles (215 mph is stored as 346 km/h, 347 km/h shows as 215 mph); a text
    /// that is no finite number is refused; a value that is no plain number shows the minimum and stays stored.
    /// The fan power ranges store whole ends kept on their side of each other, and the ejection seat 1 / 0.
    #[test]
    fn wind_writes_convert_the_speed_unit_and_keep_whole_fan_ranges() {
        use openrigcue_ui::ui::{dispatch as ui_act, Screen};
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        f7_unlock(&backend);
        let mut app = mounted_app(backend, Screen::Wind);
        let initial = detached(&h.context.state.profile.read().native_tune());
        let speed = "c-wind-default_max_speed";
        f7_html_with(&mut app, Screen::Wind, speed);
        ui_act(&mut app, "set", "Wind:default_max_speed", "347").unwrap();
        assert_eq!(f7_tune(&h, &["default_max_speed_kmh"]), LegacyValue::Int(347));
        assert_eq!(f7_attr(&openrigcue_ui::ui::screens::main(&app), speed, "value").as_deref(), Some("347"));
        for refused in ["NaN", "inf", ""] {
            let error = ui_act(&mut app, "set", "Wind:default_max_speed", refused).unwrap_err();
            assert!(error.starts_with("ValueError:"), "{refused}: {error}");
            assert_eq!(f7_tune(&h, &["default_max_speed_kmh"]), LegacyValue::Int(347));
        }
        for stored in f7_malformed() {
            f7_with(&h, &initial, &["default_max_speed_kmh"], stored.clone());
            app.refresh_docs();
            let html = openrigcue_ui::ui::screens::main(&app);
            assert_eq!(f7_attr(&html, speed, "value").as_deref(), Some("0"), "{stored:?}");
            assert!(f7_same(&f7_tune(&h, &["default_max_speed_kmh"]), &stored), "{stored:?} stays stored");
        }
        f7_with(&h, &initial, &["default_max_speed_kmh"], LegacyValue::Int(347));
        h.context.state.config.write().update(|configuration| {
            configuration.settings.app.speed_unit = openrigcue_config::settings::MeasurementUnit::MilesPerHour;
        });
        app.refresh_docs();
        assert!(!openrigcue_ui::ui::screens::wind::kmh(&app));
        let html = f7_html_with(&mut app, Screen::Wind, speed);
        assert_eq!(f7_attr(&html, speed, "value").as_deref(), Some("215"));
        ui_act(&mut app, "set", "Wind:default_max_speed", "215").unwrap();
        assert_eq!(f7_tune(&h, &["default_max_speed_kmh"]), LegacyValue::Int(346));

        for (row, prefix) in [("fan_power_perc", "power_wind"), ("fan_power_center_perc", "power_center_wind")] {
            let id = format!("c-wind-{row}");
            f7_html_with(&mut app, Screen::Wind, &id);
            let low = format!("Wind:{row}");
            let high = format!("Wind:{row}:high");
            let ends = || {
                (
                    f7_tune(&h, &[&format!("{prefix}_min_speed_perc")]),
                    f7_tune(&h, &[&format!("{prefix}_max_speed_perc")]),
                )
            };
            ui_act(&mut app, "set", &low, "0").unwrap();
            ui_act(&mut app, "set", &high, "78").unwrap();
            ui_act(&mut app, "set", &low, "1.4").unwrap();
            assert_eq!(ends(), (LegacyValue::Int(1), LegacyValue::Int(78)), "{row}");
            ui_act(&mut app, "set", &low, "90").unwrap();
            assert_eq!(ends(), (LegacyValue::Int(78), LegacyValue::Int(78)), "{row}: the low end stays below");
            ui_act(&mut app, "set", &low, "1").unwrap();
            ui_act(&mut app, "set", &high, "0").unwrap();
            assert_eq!(ends(), (LegacyValue::Int(1), LegacyValue::Int(1)), "{row}: the high end stays above");
            ui_act(&mut app, "set", &high, "78").unwrap();
            let html = openrigcue_ui::ui::screens::main(&app);
            assert_eq!(f7_attr(&html, &id, "value").as_deref(), Some("1"), "{row}");
            assert_eq!(f7_attr(&html, &format!("{id}-high"), "value").as_deref(), Some("78"), "{row}");
        }
        // The ejection seat is offered for games other than racing games.
        let flight = openrigcue_config::catalog::Catalog::builtin()
            .games()
            .iter()
            .find(|game| game.kind == openrigcue_config::catalog::GameKind::Flying)
            .unwrap();
        h.context.telemetry.publish_game_value(&h.context.state, Some(flight.id.clone()));
        openrigcue_ui::ui::tick(&mut app);
        assert!(!app.ui.session.racing);
        let ejection = "c-wind-ejection_seat";
        f7_html_with(&mut app, Screen::Wind, ejection);
        ui_act(&mut app, "set", "Wind:ejection_seat", "1").unwrap();
        assert_eq!(f7_tune(&h, &["enable_ejection_seat"]), LegacyValue::Int(1));
        assert!(f7_checked(&openrigcue_ui::ui::screens::main(&app), ejection));
        ui_act(&mut app, "set", "Wind:ejection_seat", "0").unwrap();
        assert_eq!(f7_tune(&h, &["enable_ejection_seat"]), LegacyValue::Int(0));
        f7_with(&h, &initial, &["enable_ejection_seat"], LegacyValue::Tuple(vec![]));
        app.refresh_docs();
        assert!(!f7_checked(&f7_html_with(&mut app, Screen::Wind, ejection), ejection), "a tuple shows off");
        drop(app.take_threads());
    }

    /// Tuning → IFlag: a flag's switch stores a boolean; the switch shows the stored flag (a boolean, a whole
    /// number or a "yes"/"true" text), and reading writes nothing back.
    #[test]
    fn iflag_switches_store_booleans_and_show_the_stored_flag() {
        use openrigcue_ui::ui::{components::control_id, dispatch as ui_act, Screen};
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        f7_unlock(&backend);
        let mut app = mounted_app(backend, Screen::IFlag);
        let flag = openrigcue_config::catalog::registry::Registry::builtin()
            .flags
            .iter()
            .find(|flag| flag.group != "none")
            .unwrap();
        let id = control_id("IFlag", &flag.name, "");
        let arg = format!("IFlag:{}", flag.name);
        let initial = detached(&h.context.state.profile.read().native_tune());
        f7_html_with(&mut app, Screen::IFlag, &id);
        ui_act(&mut app, "set", &arg, "1").unwrap();
        assert_eq!(f7_tune(&h, &["iflag", &flag.name]), LegacyValue::Bool(true));
        assert!(f7_checked(&openrigcue_ui::ui::screens::main(&app), &id));
        ui_act(&mut app, "set", &arg, "0").unwrap();
        assert_eq!(f7_tune(&h, &["iflag", &flag.name]), LegacyValue::Bool(false));
        assert!(!f7_checked(&openrigcue_ui::ui::screens::main(&app), &id));
        for (stored, on) in [
            (LegacyValue::Bool(true), true),
            (LegacyValue::Bool(false), false),
            (LegacyValue::Int(1), true),
            (LegacyValue::Int(0), false),
            (LegacyValue::Str("yes".into()), true),
            (LegacyValue::Str("true".into()), true),
            (LegacyValue::Str("no".into()), false),
        ] {
            f7_with(&h, &initial, &["iflag", &flag.name], stored.clone());
            app.refresh_docs();
            assert_eq!(f7_checked(&openrigcue_ui::ui::screens::main(&app), &id), on, "{stored:?}");
            assert!(f7_same(&f7_tune(&h, &["iflag", &flag.name]), &stored), "{stored:?} stays stored");
        }
        drop(app.take_threads());
    }

    /// Tuning → Routines: each routine's status reads the stored `enabled` value of the loaded tune.
    #[test]
    fn routines_status_reads_the_stored_enabled_value() {
        use openrigcue_ui::ui::Screen;
        let h = super::super::tests::harness();
        let mut app = mounted_app(harness_backend(&h), Screen::Routines);
        let initial = detached(&h.context.state.profile.read().native_tune());
        let status = |app: &openrigcue_ui::app::MyApp| {
            let html = openrigcue_ui::ui::screens::main(app);
            let at = html.find("Fanalab").expect("the Fanalab routine");
            let rest = &html[at..];
            let start = rest.find("class=\"mono\">").unwrap() + "class=\"mono\">".len();
            rest[start..start + rest[start..].find('<').unwrap()].to_owned()
        };
        for (value, expected) in [
            (LegacyValue::Bool(true), "Enabled"),
            (LegacyValue::Int(1), "Enabled"),
            (LegacyValue::Float(2.5), "Enabled"),
            (LegacyValue::Float(0.0), "Disabled"),
            (LegacyValue::Bool(false), "Disabled"),
            (LegacyValue::None, "Disabled"),
        ] {
            let mut raw = initial.clone();
            let mut routines =
                LegacyValue::from(serde_json::json!({"Fanalab":{"enabled":null,"profile":"Auto Setup"}}));
            routines
                .as_dict_mut()
                .unwrap()
                .get_mut("Fanalab")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert("enabled", value.clone());
            raw.as_dict_mut().unwrap().insert("routines", routines);
            h.context.state.profile.write().replace_native_tune(raw);
            app.refresh_docs();
            assert_eq!(status(&app), expected, "{value:?}");
        }
        drop(app.take_threads());
    }

    /// Tuning → Profiles → Export Profile: the profile in use is offered for download as
    /// `<game>_<track>.srs`, the track taken as stored (`None`, an empty or "0" text are kept; a value equal to 0
    /// leaves it out), with the shown vehicle (an empty name kept) in the file; the per-game Export of the list
    /// leaves the track out. Each export is a new download. The host setting is made a number here so the
    /// download stops before a browser would open, with the error that names it.
    #[test]
    fn export_of_the_profile_in_use_names_the_file_with_the_track() {
        use openrigcue_ui::ui::{dispatch as ui_act, Screen};
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        backend.assign_setting_value("APP", "webui_hostname", &LegacyValue::Int(0)).unwrap();
        let mut app = mounted_app(backend.clone(), Screen::Profiles);
        let newest = |app: &openrigcue_ui::app::MyApp| {
            let number = |id: &str| id.trim_start_matches("orc-download-").parse::<usize>().unwrap();
            app.downloads.iter().max_by_key(|(id, _)| number(id)).map(|(_, file)| file.clone()).unwrap()
        };
        let export = |app: &mut openrigcue_ui::app::MyApp, act: &str, arg: &str| {
            assert_eq!(
                ui_act(app, act, arg, ""),
                Err("TypeError: can only concatenate str (not \"int\") to str".into()),
                "the browser is not opened"
            );
            newest(app)
        };
        h.context.state.session.write().game = Some("ac".into());
        h.context.state.session.write().set_location_value("Track".into(), Some("Track".into()));
        assert_eq!(export(&mut app, "profile-export-current", "").0, "ac_Track.srs");
        for (location, name) in [
            (LegacyValue::None, "ac_None.srs"),
            ("".into(), "ac_.srs"),
            ("0".into(), "ac_0.srs"),
            (LegacyValue::Bool(false), "ac.srs"),
        ] {
            h.context.state.session.write().set_location_value(location, None);
            assert_eq!(export(&mut app, "profile-export-current", "").0, name);
        }
        h.context.state.session.write().set_location_value("Track".into(), Some("Track".into()));
        assert_eq!(export(&mut app, "profile-export", "ac").0, "ac.srs", "the list's Export omits the track");
        assert_eq!(backend.profile_export("ac", &LegacyValue::Int(0)).unwrap().1, "ac.srs");
        h.context.state.session.write().set_vehicle_value("".into(), Some("".into()));
        let (_, bytes) = export(&mut app, "profile-export-current", "");
        assert_eq!(
            openrigcue_config::profiles::read_profile_file(&bytes).unwrap().vehicle_value(),
            &LegacyValue::Str("".into())
        );
        assert_eq!(app.downloads.len(), 7, "each export is a download of its own");
        drop(app.take_threads());
    }

    /// The opening tag of the rendered control `id` (e.g. `c-app-start_with_login`).
    fn f8_control_tag<'a>(html: &'a str, id: &str) -> &'a str {
        let start = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("no control {id}: {html}"));
        let start = html[..start].rfind('<').unwrap();
        &html[start..start + html[start..].find('>').unwrap()]
    }

    /// The `value` the rendered control `id` shows.
    fn f8_control_value(html: &str, id: &str) -> String {
        let tag = f8_control_tag(html, id);
        let start = tag.find(" value=\"").unwrap_or_else(|| panic!("no value in {tag}")) + " value=\"".len();
        tag[start..start + tag[start..].find('"').unwrap()].to_owned()
    }

    #[test]
    fn profile_manager_game_edit_reaches_the_native_context_with_integer_zero() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Profiles);
        assert!(
            openrigcue_ui::ui::screens::main(&app).contains("data-act=\"profile-edit\" data-arg=\"ac\""),
            "Tuning → Profiles lists the game with its Edit button"
        );

        openrigcue_ui::ui::dispatch(&mut app, "profile-edit", "ac", "").unwrap();

        assert_eq!(app.ui.screen, openrigcue_ui::ui::Screen::Home, "Edit leaves the list for the tuning screen");
        let controls = h.context.state.controls.snapshot();
        let editing = controls.profile_edit.expect("the game Edit button must start an edit context");
        assert_eq!(controls.forced_game.as_deref(), Some("ac"));
        assert_eq!(editing.game, "ac");
        assert_eq!(editing.vehicle, None);
        assert!(editing.started_without_listener);
        assert_eq!(h.context.state.session.read().vehicle_value(), LegacyValue::Int(0));

        h.context.state.session.write().listener = Some("ac".into());
        for value in [
            LegacyValue::Int(2),
            LegacyValue::Float(1.5),
            LegacyValue::Float(f64::INFINITY),
            LegacyValue::None,
            "".into(),
            "Named car".into(),
        ] {
            backend.begin_profile_edit("acc", &value).unwrap();
            let controls = h.context.state.controls.snapshot();
            assert_eq!(controls.forced_game.as_deref(), Some("acc"));
            assert!(!controls.profile_edit.as_ref().unwrap().started_without_listener);
            assert_eq!(h.context.state.session.read().vehicle_value(), value);
        }
        assert!(h.context.state.session.read().game.is_none(), "detection still owns game_running");
        drop(app.take_threads());
    }

    #[test]
    fn profile_manager_uses_live_native_rows_without_decoding_unrelated_profiles() {
        use openrigcue_config::import::{
            sqlitedict,
            value::{Dict, Key, LegacyValue},
        };
        use openrigcue_config::profiles::read_profile_file;
        use openrigcue_config::settings::{paths::ConfigDir, Configuration};
        let h = super::super::tests::harness();
        let dir = ConfigDir::at(h._tmp.path().join("manager-native"));
        Configuration::first_run().save(&dir).unwrap();
        *h.context.state.config.write() = crate::store::ConfigStore::load(dir.clone()).unwrap();
        let mut row = h.context.state.config.read().profile_row("ac").unwrap().unwrap();
        let mut numeric_car = row.clone();
        numeric_car.as_dict_mut().unwrap().insert("marker", 43.into());
        let mut text_car = row.clone();
        text_car.as_dict_mut().unwrap().insert("marker", 61.into());
        row.as_dict_mut().unwrap().insert("marker", 99.into());
        let mut vehicles = Dict::new();
        vehicles.insert(Key::Int(2), numeric_car);
        vehicles.insert("2", text_car);
        row.as_dict_mut().unwrap().insert("vehicles", vehicles.into());
        sqlitedict::set_item(&dir.profiles_file(), "unnamed", "ac", &row).unwrap();
        let database = rusqlite::Connection::open(dir.profiles_file()).unwrap();
        database.execute("INSERT INTO unnamed (key,value) VALUES ('opaque',?)", [vec![0xff_u8]]).unwrap();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        assert_eq!(backend.profile_row("ac").unwrap(), Some(row.clone()));
        assert!(backend.profile_row("missing").unwrap().is_none());
        assert!(backend.profile_row("opaque").is_err());
        for (vehicle, expected_name, marker) in [
            (LegacyValue::Int(2), "ac.srs", 99),
            (LegacyValue::Str("2".into()), "ac_2.srs", 61),
            (LegacyValue::Int(0), "ac.srs", 99),
        ] {
            let (bytes, name) = backend.profile_export("ac", &vehicle).unwrap();
            assert_eq!(name, expected_name);
            let file = read_profile_file(&bytes).unwrap();
            assert_eq!(file.vehicle_value(), &vehicle);
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
            let contents: Value = serde_json::from_reader(archive.by_index(0).unwrap()).unwrap();
            assert_eq!(contents["marker"], marker);
        }
        // Tuning → Profiles reads each game's live row: the stored text-named vehicle is listed (a vehicle stored
        // under a number has no name to list), and the undecodable row of an unrelated key is never read.
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Profiles);
        let vehicles = |app: &openrigcue_ui::app::MyApp, game: &str| {
            app.ui.profiles.entries.iter().find(|entry| entry.game == game).map(|entry| entry.vehicles.clone())
        };
        assert_eq!(vehicles(&app, "ac"), Some(vec!["2".to_owned()]));
        assert!(vehicles(&app, "acc").is_some(), "the games after it are listed too");
        let html = openrigcue_ui::ui::screens::main(&app);
        assert!(html.contains("data-act=\"profile-edit\" data-arg=\"ac^2\""), "{html}");
        assert!(html.contains("data-act=\"profile-edit\" data-arg=\"acc\""));
        row.as_dict_mut().unwrap().insert("vehicles", Dict::new().into());
        sqlitedict::set_item(&dir.profiles_file(), "unnamed", "ac", &row).unwrap();
        openrigcue_ui::ui::navigate(&mut app, openrigcue_ui::ui::Screen::Profiles, 0);
        assert_eq!(vehicles(&app, "ac"), Some(Vec::new()), "opening the list again reads the live row again");
        assert!(!openrigcue_ui::ui::screens::main(&app).contains("data-arg=\"ac^2\""));
        drop(app.take_threads());
        backend.close_opened_files();
    }

    #[test]
    fn startup_switches_write_launcher_flags_without_ini_or_profile_commit() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        let h = super::super::tests::harness();
        let dir = openrigcue_config::settings::paths::ConfigDir::at(h._tmp.path().join("flag-config"));
        let store = crate::store::ConfigStore::load(dir.clone()).unwrap();
        store.save().unwrap();
        *h.context.state.config.write() = store;
        sqlitedict::set_item(&dir.registry_file(), "registry", "StartWithWindows", &LegacyValue::from("0")).unwrap();
        rusqlite::Connection::open(dir.registry_file())
            .unwrap()
            .execute("UPDATE registry SET value=? WHERE key='ShowSplashScreen'", [vec![255_u8]])
            .unwrap();
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        // Settings → App → Startup Options.
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::App);
        openrigcue_ui::ui::dispatch(&mut app, "section", "", "1").unwrap();
        let html = openrigcue_ui::ui::screens::main(&app);
        for id in ["c-app-start_with_login", "c-app-show_splash_screen"] {
            assert!(f8_control_tag(&html, id).contains("type=\"checkbox\""), "{id} is a switch");
        }
        let before = std::fs::read(dir.settings_file()).unwrap();
        let original = h.context.state.config.read().profile_row("emulator").unwrap().unwrap();
        let mut pending = original.clone();
        pending.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(73));
        h.context.state.config.write().stage_game_row("emulator", Some(pending.clone())).unwrap();
        openrigcue_ui::ui::dispatch(&mut app, "set", "App:start_with_login", "1").unwrap();
        openrigcue_ui::ui::dispatch(&mut app, "set", "App:show_splash_screen", "1").unwrap();
        assert_eq!(backend.launcher_flag("StartWithWindows").unwrap(), Some("1".into()));
        assert_eq!(
            backend.launcher_flag("ShowSplashScreen").unwrap(),
            Some("1".into()),
            "set_reg must not decode the previous bad value"
        );
        assert_eq!(std::fs::read(dir.settings_file()).unwrap(), before);
        assert!(h.context.state.config.read().profile_row("emulator").unwrap() == Some(pending));
        assert!(sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap() == Some(original));
        rusqlite::Connection::open(dir.registry_file()).unwrap().execute_batch(
            "CREATE TRIGGER reject_flag BEFORE INSERT ON registry BEGIN SELECT RAISE(ABORT,'fixture registry failure'); END;"
        ).unwrap();
        openrigcue_ui::ui::dispatch(&mut app, "set", "App:start_with_login", "0")
            .expect("a failed launcher write is not an error of the switch");
        assert_eq!(backend.launcher_flag("StartWithWindows").unwrap(), Some("1".into()));
        assert!(
            f8_control_tag(&openrigcue_ui::ui::screens::main(&app), "c-app-start_with_login").contains(" checked"),
            "the switch shows the stored value after a failed write"
        );
        assert_eq!(std::fs::read(dir.settings_file()).unwrap(), before);
        drop(app.take_threads());
        backend.close_opened_files();
    }

    #[test]
    fn shaker_operations_use_live_rows_and_an_independent_loaded_snapshot() {
        use openrigcue_config::import::{migrate::Platform, sqlitedict, value::LegacyValue};
        fn engine(mut value: LegacyValue, level: f64) -> LegacyValue {
            let channel = value.as_dict_mut().unwrap().get_mut("FL").unwrap().as_dict_mut().unwrap();
            channel.insert("unknown", LegacyValue::None);
            channel.get_mut("sfx").unwrap().as_dict_mut().unwrap().insert("engine", LegacyValue::Float(level));
            value
        }
        let h = super::super::tests::harness();
        let directory = openrigcue_config::settings::paths::ConfigDir::at(h._tmp.path().join("raw-shakers"));
        let initial = crate::store::ConfigStore::load(directory.clone()).unwrap();
        initial.save().unwrap();
        drop(initial);
        *h.context.state.config.write() =
            crate::store::ConfigStore::open(directory.clone(), Platform::Windows, None).unwrap();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        backend.load_shaker_effects().unwrap();
        let first = engine(backend.shaker_effects().unwrap(), 3.0);
        let second = engine(first.clone(), 7.0);
        let third = engine(first.clone(), 9.0);
        sqlitedict::set_item(&directory.metadata_file(), "shakers", "Source", &first).unwrap();
        backend.assign_shaker_profile("Source").unwrap();
        backend.load_shaker_effects().unwrap();
        let connection = rusqlite::Connection::open(directory.metadata_file()).unwrap();
        connection.execute("INSERT INTO shakers (key,value) VALUES ('Opaque',?)", [vec![255_u8]]).unwrap();
        sqlitedict::set_item(&directory.metadata_file(), "shakers", "Source", &second).unwrap();
        assert!(
            h.context.state.config.read().config().shaker_layouts.get("Source").is_none(),
            "selected reads do not need a complete compatibility cache"
        );
        backend.copy_shaker_profile("Source", "New").unwrap();
        assert_eq!(backend.metadata_row("shakers", "New").unwrap(), Some(second.clone()));
        assert_eq!(backend.shaker_effects().unwrap(), first, "copy alone neither selects nor reloads");
        assert_eq!(h.context.state.config.read().settings().shakers.speaker_layout, "Source");
        let keys = backend.metadata_keys("shakers").unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert!(keys.iter().any(|key| key == "Opaque") && keys.iter().any(|key| key == "New"));
        sqlitedict::set_item(&directory.metadata_file(), "shakers", "Source", &third).unwrap();
        backend.load_shaker_effects().unwrap();
        backend.assign_shaker_profile("New").unwrap();
        assert_eq!(backend.shaker_effects().unwrap(), third, "list reload precedes the later name assignment");
        assert_eq!(backend.metadata_row("shakers", "New").unwrap(), Some(second));
        super::super::ui_state::store_shaker_effects(&h.context).unwrap();
        assert_eq!(
            backend.metadata_row("shakers", "New").unwrap(),
            Some(third.clone()),
            "Save writes the loaded global under the current name"
        );
        backend.assign_shaker_profile("Missing").unwrap();
        assert!(backend.load_shaker_effects().is_err());
        assert_eq!(backend.shaker_effects().unwrap(), third, "a failed selected read retains the prior global object");
        let opaque: Vec<u8> =
            connection.query_row("SELECT value FROM shakers WHERE key='Opaque'", [], |row| row.get(0)).unwrap();
        assert_eq!(opaque, [255]);
        backend.close_opened_files();
    }

    #[test]
    fn runtime_joystick_dropdown_uses_the_database_after_lazy_startup() {
        use openrigcue_config::import::{migrate::Platform, sqlitedict, value::LegacyValue};
        let h = super::super::tests::harness();
        let directory = openrigcue_config::settings::paths::ConfigDir::at(h._tmp.path().join("lazy-joystick"));
        let initial = crate::store::ConfigStore::load(directory.clone()).unwrap();
        initial.save().unwrap();
        drop(initial);
        sqlitedict::set_item(&directory.metadata_file(), "joystick", "Wheel", &LegacyValue::Dict(Default::default()))
            .unwrap();
        let store = crate::store::ConfigStore::open(directory, Platform::Windows, None).unwrap();
        assert!(store.config().joysticks.0.is_empty());
        *h.context.state.config.write() = store;
        let backend = Arc::new(DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        });
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Joystick);
        assert_eq!(app.ui.devices.joystick.profiles, ["Default", "Wheel"]);
        let html = openrigcue_ui::ui::screens::main(&app);
        assert!(html.contains(">Wheel</option>"), "the profile choice offers the stored profile: {html}");
        drop(app.take_threads());
        backend.close_opened_files();
    }

    #[test]
    fn ui_metadata_boundaries_use_live_rows_and_lazy_decode_failures() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        let harness = super::super::tests::harness();
        let directory = openrigcue_config::settings::paths::ConfigDir::at(harness._tmp.path().join("config"));
        let store = crate::store::ConfigStore::load(directory).unwrap();
        store.save().unwrap();
        *harness.context.state.config.write() = store;
        let path = harness.context.state.config.read().dir().unwrap().metadata_file();
        let live = LegacyValue::from(serde_json::json!({"external":23}));
        sqlitedict::set_item(&path, "joystick", "Default", &live).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute("INSERT INTO joystick (key,value) VALUES ('opaque',?)", [vec![255_u8]]).unwrap();
        drop(connection);
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: harness.context.ui.clone(),
            context: harness.context.clone(),
        };
        assert_eq!(backend.metadata_row("joystick", "Default").unwrap(), Some(live.clone()));
        assert!(backend.metadata_contains("joystick", "opaque").unwrap());
        assert!(backend.metadata_row("joystick", "opaque").is_err());
        let mut rows = backend.metadata_rows("joystick").unwrap();
        assert_eq!(rows.next().unwrap().unwrap(), ("Default".into(), live));
        assert!(rows.next().unwrap().is_err());
        assert!(rows.next().is_none());
        assert!(backend.get("joystick/db").is_err(), "whole-table API still reports a malformed row");
        backend.close_opened_files();
    }

    #[test]
    fn app_log_callback_applies_every_level_after_the_native_assignment() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let mut context = h.context.clone();
        let levels = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = levels.clone();
        let config = context.state.config.clone();
        let settings = config.read().settings().clone();
        context.ui = Arc::new(super::super::ui_state::UiServices::new(
            &settings,
            Some(Arc::new(move |level| {
                assert_eq!(
                    config.read().settings().native_snapshot().get("APP").unwrap().item_owned("log_level").unwrap(),
                    LegacyValue::Int(i64::from(level)),
                    "the logger sees the completed native assignment"
                );
                observed.lock().unwrap().push(level);
            })),
            None,
        ));
        let backend =
            DaemonBackend { runtime: Runtime::Own(OnceLock::new()), ui: context.ui.clone(), context: context.clone() };
        for level in [10, 10, 50] {
            backend.assign_app_setting("log_level", Value::from(level)).unwrap();
        }
        assert_eq!(*levels.lock().unwrap(), vec![10, 10, 50]);
        for (unit, shown) in [("K", "kmh"), ("M", "mph")] {
            let view = backend.assign_app_setting("measurement_unit", Value::from(unit)).unwrap();
            assert_eq!(view["app"]["speed_unit"], shown);
            assert_eq!(
                context
                    .state
                    .config
                    .read()
                    .settings()
                    .native_snapshot()
                    .get("APP")
                    .unwrap()
                    .item_owned("measurement_unit"),
                Ok(LegacyValue::from(unit))
            );
        }
        context.state.config.write().update(|config| {
            config.settings.edit_native(|raw| raw.insert("APP", LegacyValue::None));
        });
        assert!(backend.assign_app_setting("log_level", Value::from(20)).is_err());
        assert_eq!(*levels.lock().unwrap(), vec![10, 10, 50], "no logging call after a failed assignment");
    }

    #[test]
    fn window_events_assign_canonical_native_values_without_typed_coercion() {
        use openrigcue_model::native_value::LegacyValue;
        let h = super::super::tests::harness();
        let backend = DaemonBackend {
            runtime: Runtime::Own(OnceLock::new()),
            ui: h.context.ui.clone(),
            context: h.context.clone(),
        };
        for key in ["window_width", "window_height", "pixel_ratio"] {
            for value in [
                serde_json::json!("1280"),
                serde_json::json!(1280),
                serde_json::json!(1.25),
                serde_json::json!(true),
                Value::Null,
                serde_json::json!([1, null]),
                serde_json::json!({"x":1}),
            ] {
                let view = backend.assign_window_setting(key, value.clone()).unwrap();
                let raw = h.context.state.config.read().settings().native_snapshot();
                assert_eq!(
                    raw.get("APP").unwrap().as_dict().unwrap().get(key),
                    Some(&LegacyValue::from(value.clone())),
                    "{key}/{value}"
                );
                assert_eq!(view["app"][if key == "pixel_ratio" { "device_pixel_ratio" } else { key }], value);
            }
        }
        for app in [LegacyValue::None, LegacyValue::List(Vec::new()), LegacyValue::Int(17), LegacyValue::from("bad")] {
            h.context
                .state
                .config
                .write()
                .update(|configuration| configuration.settings.edit_native(|raw| raw.insert("APP", app.clone())));
            let before = h.context.state.config.read().settings().native_snapshot();
            assert!(backend.assign_window_setting("window_width", serde_json::json!(1280)).is_err());
            assert_eq!(h.context.state.config.read().settings().native_snapshot(), before);
        }
    }

    #[test]
    fn pagehide_file_close_rolls_back_pending_profile_and_removes_the_journal() {
        use openrigcue_config::import::sqlitedict;
        let directory = tempfile::tempdir().unwrap();
        let h = super::super::tests::harness();
        let mut context = h.context.clone();
        context.state.config = crate::runtime::Shared::new(
            crate::store::ConfigStore::load(openrigcue_config::settings::paths::ConfigDir::at(directory.path()))
                .unwrap(),
        );
        context.state.config.read().save().unwrap();
        let path = directory.path().join("profile.db");
        let before = sqlitedict::read_table(&path, "unnamed").unwrap();
        context.state.config.write().stage_game_row("fixture-unsaved", Some("unsaved".into())).unwrap();
        let journal = directory.path().join("profile.db-journal");
        assert!(journal.exists(), "staged row must have a live transaction journal");
        let backend = DaemonBackend { runtime: Runtime::Own(OnceLock::new()), ui: context.ui.clone(), context };
        backend.close_opened_files();
        assert!(!journal.exists(), "close must roll back before SIGKILL, not leave a hot journal");
        assert_eq!(sqlitedict::read_table(&path, "unnamed").unwrap(), before);
    }

    /// The UI on the service's own routes: the first page load mounts the interface, one main-loop pass loads
    /// the first-run profile, every screen of the sections is offered, and a Wind value the browser sends
    /// changes the service's tune without a profile reload.
    #[test]
    fn ui_runs_on_the_service() {
        use openrigcue_ui::ui::{Group, Screen};
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), Screen::Home);
        app.main_loop_pass();
        assert!(app.ml.fatal_error.is_none(), "{:?}", app.ml.fatal_error);
        for (group, screens) in [
            (
                Group::Tuning,
                &[
                    Screen::Wind,
                    Screen::Shake,
                    Screen::Motion,
                    Screen::Belt,
                    Screen::Sim3d,
                    Screen::IFlag,
                    Screen::Led,
                    Screen::Routines,
                ][..],
            ),
            (Group::Games, &[Screen::Pc, Screen::Console, Screen::Telemetry][..]),
            (Group::Hardware, &[Screen::Hardware, Screen::Joystick, Screen::Compensation][..]),
            (Group::Settings, &[Screen::App][..]),
        ] {
            let offered = Screen::of_group(group, &app);
            for screen in screens {
                assert!(offered.contains(screen), "missing {group:?}/{screen:?}");
            }
        }
        // Wind → Speed.
        openrigcue_ui::ui::navigate(&mut app, Screen::Wind, 4);
        let stored = backend.get("profile").unwrap()["tune"]["speed_factor"].as_f64().unwrap();
        assert_ne!(stored, 10.0);
        let shown = f8_control_value(&openrigcue_ui::ui::screens::main(&app), "c-wind-speed_factor");
        assert_eq!(shown.parse::<f64>().unwrap(), stored);
        // In stand-by the page is locked; with stand-by editing allowed the value reaches the service.
        openrigcue_ui::ui::dispatch(&mut app, "set", "Wind:speed_factor", "10").unwrap();
        assert_eq!(backend.get("profile").unwrap()["tune"]["speed_factor"].as_f64(), Some(stored));
        h.context.state.config.write().update(|c| c.settings.features.standby_tuning_editable = true);
        openrigcue_ui::ui::dispatch(&mut app, "set", "Wind:speed_factor", "10").unwrap();
        assert_eq!(backend.get("profile").unwrap()["tune"]["speed_factor"].as_f64(), Some(10.0));
        let load = backend.get("status").unwrap()["profile"]["load_generation"].clone();
        app.main_loop_pass();
        assert!(app.ml.fatal_error.is_none(), "{:?}", app.ml.fatal_error);
        assert_eq!(backend.get("status").unwrap()["profile"]["load_generation"], load);
        assert_eq!(f8_control_value(&openrigcue_ui::ui::screens::main(&app), "c-wind-speed_factor"), "10");
        assert!(!app.gui.full_html().is_empty());
        drop(app.take_threads());
    }

    #[test]
    fn independent_ui_services_complete_concurrently() {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..4 {
                        ui_runs_on_the_service();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    }

    /// A loaded profile with HapticR enabled: the main loop writes the missing HapticR defaults into the shared
    /// active profile (the fade range 94 – 100 and the thresholds 30 / 50 / 40), and the app's tune is the
    /// service's.
    #[test]
    fn hapticr_profile_load_writes_missing_defaults_into_the_shared_profile() {
        use openrigcue_config::profiles::{SaveTarget, Vehicle};
        use serde_json::json;

        let h = super::super::tests::harness();
        {
            let mut store = h.context.state.config.write();
            store.update(|configuration| {
                configuration.settings.app.enable_hapticr = true;
                let mut raw = configuration.profiles.resolve_native("emulator", &Vehicle::Unknown).unwrap().0;
                raw.as_dict_mut().unwrap().insert("haptic_effects", json!({"rpm_start_rumble":12,"obsolete":4}).into());
                configuration.profiles.save_native("emulator", &SaveTarget::Game, raw).unwrap();
            });
            *h.context.state.profile.write() = crate::profile::ActiveProfile::standby(&store);
        }
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Home);
        app.main_loop_pass();
        assert!(app.ml.fatal_error.is_none(), "{:?}", app.ml.fatal_error);
        let shared = backend.get("profile").unwrap()["tune"]["haptic_effects"].clone();
        assert_eq!(shared, app.docs.tune["haptic_effects"]);
        for (key, value) in [
            ("rpm_start_fading", json!(94)),
            ("rpm_stop_fading", json!(100)),
            ("wheel_slip_min_activation", json!(30.0)),
            ("wheel_lock_min_activation", json!(50.0)),
            ("traction_loss_min_activation", json!(40.0)),
        ] {
            assert_eq!(shared[key].as_f64(), value.as_f64(), "{key}");
        }
        let native = h.context.state.profile.read().native_tune().item_owned("haptic_effects").unwrap();
        assert_eq!(native.item_owned("rpm_start_fading").unwrap(), LegacyValue::Int(94), "kept as an integer");
        assert!(openrigcue_ui::ui::Screen::HapticR.available(&app));
        drop(app.take_threads());
    }

    /// Hardware → the SUB speaker's Test: the output test dialog's Start plays the speaker's test sound while a
    /// sound card is chosen; a speaker level that is not a number fails the start (a warning) before any sound,
    /// and with the sound card "None" nothing plays.
    #[test]
    fn hardware_speaker_test_plays_with_a_sound_card_and_keeps_the_eager_level_failure() {
        use openrigcue_effects::shakers::rig::Voice;
        let h = super::super::tests::harness();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), openrigcue_ui::ui::Screen::Hardware);
        let playing = || h.context.shakers.lock().system.mixer().lock().unwrap().is_playing(Voice::SpeakerTest);
        let soundcard = |value: LegacyValue| {
            h.context.state.config.write().update(|configuration| {
                configuration.settings.edit_native(|raw| {
                    raw.get_mut("SHAKEBOX").unwrap().assign_item("soundcard", value).unwrap();
                });
            });
        };
        let original = detached(&h.context.state.profile.read().native_tune());
        let mut bad = original.clone();
        bad.as_dict_mut().unwrap().get_mut("shaker_volume").unwrap().assign_item("fl", "7".into()).unwrap();
        h.context.state.profile.write().replace_native_tune(bad);
        soundcard(LegacyValue::from("Fixture sound card"));
        openrigcue_ui::ui::dispatch(&mut app, "action", "Hardware:sub:0", "").unwrap();
        assert!(openrigcue_ui::ui::dialogs::render(&app).contains("data-arg=\"start\""), "the output test dialog");
        let error = openrigcue_ui::ui::dispatch(&mut app, "dialog-button", "start", "").unwrap_err();
        assert_eq!(error, "TypeError: unsupported operand type(s) for /: 'str' and 'float'");
        assert!(!playing());
        h.context.state.profile.write().replace_native_tune(original);
        soundcard(LegacyValue::from("None"));
        openrigcue_ui::ui::dispatch(&mut app, "dialog-button", "start", "").unwrap();
        assert!(!playing(), "no test sound without a sound card");
        soundcard(LegacyValue::from("Fixture sound card"));
        openrigcue_ui::ui::dispatch(&mut app, "dialog-button", "start", "").unwrap();
        assert!(playing());
        drop(app.take_threads());
    }

    /// The optional screens (HapticR, Debug) are offered and drawn against the actual service contracts, and the
    /// Wind max speed shown in mph survives repeated main-loop passes without a write.
    #[test]
    fn optional_pages_and_mph_survive_repeated_service_passes() {
        use openrigcue_ui::ui::{Group, Screen};
        let h = super::super::tests::harness();
        h.context.state.config.write().update(|c| {
            c.settings.app.enable_hapticr = true;
            c.settings.app.speed_unit = openrigcue_config::settings::MeasurementUnit::MilesPerHour;
            c.settings.features.show_debug_page = true;
        });
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), Screen::Home);
        for _ in 0..3 {
            app.main_loop_pass();
            assert!(app.ml.fatal_error.is_none(), "{:?}", app.ml.fatal_error);
        }
        for (group, screen) in [(Group::Tuning, Screen::HapticR), (Group::Settings, Screen::Debug)] {
            assert!(Screen::of_group(group, &app).contains(&screen), "missing {group:?}/{screen:?}");
            openrigcue_ui::ui::navigate(&mut app, screen, 0);
            assert_eq!(app.ui.screen, screen);
            assert!(!openrigcue_ui::ui::screens::main(&app).is_empty());
        }
        // Wind → Max Speed.
        openrigcue_ui::ui::navigate(&mut app, Screen::Wind, 2);
        let html = openrigcue_ui::ui::screens::main(&app);
        assert!(html.contains("Mph"), "{html}");
        let shown = f8_control_value(&html, "c-wind-default_max_speed");
        let stored = backend.get("profile").unwrap()["tune"]["default_max_speed_kmh"].clone();
        assert_eq!(shown.parse::<f64>().unwrap(), (stored.as_f64().unwrap() * 0.621_371).trunc(), "shown in mph");
        app.main_loop_pass();
        assert!(app.ml.fatal_error.is_none(), "{:?}", app.ml.fatal_error);
        assert_eq!(f8_control_value(&openrigcue_ui::ui::screens::main(&app), "c-wind-default_max_speed"), shown);
        assert_eq!(backend.get("profile").unwrap()["tune"]["default_max_speed_kmh"], stored);
        drop(app.take_threads());
    }

    /// The harness on a configuration folder in a temporary directory, written once as after a first start,
    /// so the service's settings file is on disk.
    fn saved_harness() -> (super::super::tests::Harness, tempfile::TempDir) {
        use crate::state::{OriginalPlatform, ServiceState};
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::ConfigStore::load(openrigcue_config::settings::paths::ConfigDir::at(
            tmp.path().join("config"),
        ))
        .unwrap();
        store.save().unwrap();
        let state = ServiceState::new(store, OriginalPlatform::Windows);
        let mut h = super::super::tests::harness();
        h.context.motion = crate::motion::MotionHandle::for_settings(
            state.config.read().settings(),
            Box::new(crate::motion::testing::OfflineHost::default()),
        );
        h.context.shakers = crate::shakers::ShakersHandle::new(
            &state,
            openrigcue_effects::shakers::SampleBank::synthesized(),
            Box::new(crate::shakers::SilentBeep),
            false,
        );
        h.context.joystick = crate::joystick::JoystickService::new(
            state.clone(),
            Default::default(),
            crate::runtime::Shared::new("Default".into()),
        );
        h.context.state = state;
        (h, tmp)
    }

    /// The changes of Settings → App (units, theme, startup page, log level) and a shortcut are kept in memory;
    /// the next navigation writes them all to the settings file, so quitting then loses nothing.
    #[test]
    fn settings_changes_reach_the_settings_file_at_the_next_navigation() {
        use openrigcue_config::settings::{MeasurementUnit, Settings};
        use openrigcue_ui::spec::Widget;
        use openrigcue_ui::ui::{dialogs::Dialog, dispatch, Screen, Theme};
        let (h, _tmp) = saved_harness();
        let file = h.context.state.config.read().dir().unwrap().settings_file();
        let backend = harness_backend(&h);
        let mut app = mounted_app(backend.clone(), Screen::App);
        let page = openrigcue_ui::ui::screens::app::page();
        let choice = |id: &str, value: &Value| {
            let Some(Widget::Select { options }) = page.control(id).map(|c| &c.widget) else { panic!("{id}") };
            options.iter().position(|o| &o.value == value).unwrap().to_string()
        };
        let stored = Settings::load(&file).unwrap();
        let level = [10, 20, 30, 40, 50].into_iter().find(|l| *l != stored.app.log_level).unwrap();
        let apex = Theme::ALL.iter().position(|t| *t == Theme::Apex).unwrap().to_string();
        dispatch(&mut app, "set", "App:measurement_unit", "1").unwrap();
        dispatch(&mut app, "set", "App:theme", &apex).unwrap();
        dispatch(&mut app, "set", "App:startup_page", &choice("startup_page", &serde_json::json!("Motion"))).unwrap();
        dispatch(&mut app, "set", "App:log_level", &choice("log_level", &serde_json::json!(level))).unwrap();
        openrigcue_ui::ui::shortcuts::open(&mut app, &["joystick_switch".to_owned()]).unwrap();
        let Some(Dialog::Shortcut(editor)) = app.ui.dialog.as_mut() else { panic!("the shortcut editor is open") };
        editor.keys = "ctrl+alt+k".into();
        dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert_eq!(Settings::load(&file).unwrap(), stored, "the changes are in memory only");
        dispatch(&mut app, "nav", "Tuning", "").unwrap();
        let written = Settings::load(&file).unwrap();
        assert_eq!(written.app.speed_unit, MeasurementUnit::MilesPerHour);
        assert_eq!(written.app.theme, "apex");
        assert_eq!(written.app.start_page, "Motion");
        assert_eq!(written.app.log_level, level);
        assert_eq!(written.shortcuts.bindings["joystick_switch"].keys, "ctrl+alt+k");
        drop(app.take_threads());
    }
}

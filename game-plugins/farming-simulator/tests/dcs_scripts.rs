//! Run the authored DCS callbacks against a software SDK and UDP endpoint.
use mlua::{Function, Lua, Table};

const HOOK: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/openrigcue-config/src/patch/builtin/openrigcue-dcs-hook.lua"
));
const EXPORT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/openrigcue-config/src/patch/builtin/openrigcue-dcs-export.lua"
));
const SDK: &str = r#"
calls = {}
packets = {}
closed = 0
aircraft_name = "fixture"
function note(name) calls[#calls + 1] = name end
lfs = {currentdir = function() return "fixture" end}
package.preload.socket = function()
    return {try = function(value) assert(value) end, udp = function()
        return {sendto = function(self, packet, host, port)
            packets[#packets + 1] = {packet, host, port}
            return #packet
        end, close = function() closed = closed + 1 end}
    end}
end
Export = {
    LoGetEngineInfo = function() note("engine"); return {RPM = {left = 64.75}} end,
    LoGetIndicatedAirSpeed = function() note("indicated"); return 2 end,
    LoGetTrueAirSpeed = function() note("true_speed"); return 3.6 end,
    LoGetAccelerationUnits = function() note("acceleration"); return {x=1.25,y=-2.5,z=3.75} end,
    LoGetADIPitchBankYaw = function() note("attitude"); return 0.125, -0.25, 0.5 end,
    LoGetAngularVelocity = function() note("angular"); return {x=1.5,y=2.5,z=3.5} end,
    LoGetSelfData = function() note("aircraft"); return {Name=aircraft_name} end,
    LoGetMechInfo = function() note("mechanisms"); return {canopy={value=0.25},gear={value=1}} end,
    LoGetSnares = function() note("snares"); return {} end,
    LoGetPayloadInfo = function() note("payload"); return {Cannon={shells=123}} end,
    GetDevice = function(index) note("device:" .. index); return {get_argument_value = function(self, index)
        note("gauge:" .. index); return 0.625
    end} end
}
for name, value in pairs(Export) do _G[name] = value end
DCS = {setUserCallbacks = function(value) callbacks = value end}
"#;

fn loaded(hook: bool) -> (Lua, Function, Function) {
    let lua = Lua::new();
    lua.load(SDK).exec().unwrap();
    lua.load(if hook { HOOK } else { EXPORT }.replace("^port^", "47777")).exec().unwrap();
    let (start, frame, stop): (Function, Function, Function) = if hook {
        let callbacks: Table = lua.globals().get("callbacks").unwrap();
        (
            callbacks.get("onSimulationStart").unwrap(),
            callbacks.get("onSimulationFrame").unwrap(),
            callbacks.get("onSimulationStop").unwrap(),
        )
    } else {
        (
            lua.globals().get("LuaExportStart").unwrap(),
            lua.globals().get("LuaExportAfterNextFrame").unwrap(),
            lua.globals().get("LuaExportStop").unwrap(),
        )
    };
    start.call::<()>(()).unwrap();
    (lua, frame, stop)
}

#[test]
fn callbacks_emit_the_complete_datagram_and_close_the_endpoint() {
    for hook in [false, true] {
        let (lua, frame, stop) = loaded(hook);
        frame.call::<()>(()).unwrap();
        let packets: Table = lua.globals().get("packets").unwrap();
        assert_eq!(packets.raw_len(), 1);
        let packet: Table = packets.get(1).unwrap();
        assert_eq!(packet.get::<String>(1).unwrap(), "1.00,64.75,3.60,1.25000,-2.50000,3.75000,0.12500,-0.25000,0.50000,3.50000,1.50000,2.50000,fixture,0.25,1,123.00000");
        assert_eq!(packet.get::<String>(2).unwrap(), "127.0.0.1");
        assert_eq!(packet.get::<u16>(3).unwrap(), 47777);
        let calls: Table = lua.globals().get("calls").unwrap();
        let calls: Vec<String> = calls.sequence_values().collect::<mlua::Result<_>>().unwrap();
        let mut expected = vec![
            "engine",
            "indicated",
            "true_speed",
            "acceleration",
            "attitude",
            "angular",
            "aircraft",
            "mechanisms",
            "snares",
            "payload",
        ];
        if hook {
            expected.push("device:0");
        }
        assert_eq!(calls, expected);
        stop.call::<()>(()).unwrap();
        assert_eq!(lua.globals().get::<i64>("closed").unwrap(), 1);
        if !hook {
            let next: Function = lua.globals().get("LuaExportActivityNextEvent").unwrap();
            assert_eq!(next.call::<f64>(12.5).unwrap(), 12.5);
        }
    }
}

#[test]
fn hook_uses_each_aircrafts_gauge_index_and_scale() {
    for (aircraft, index, factor) in [
        ("Mi-8MT", 42, 100.0),
        ("A-10C", 78, 100.0),
        ("UH-1H", 123, 100.0),
        ("Ka-50", 52, 100.0),
        ("SA342M", 52, 100.0),
        ("SA342L", 52, 100.0),
        ("SA342Mistral", 52, 100.0),
        ("SA342Minigun", 52, 100.0),
        ("P-51D", 23, 151.0),
        ("P-51D-30-NA", 23, 151.0),
        ("TF-51D", 23, 151.0),
        ("FW-190D9", 47, 1.0),
        ("Bf-109K-4", 29, 1.0),
        ("SpitfireLFMkIX", 37, 1.0),
        ("SpitfireLFMkIXCW", 37, 1.0),
        ("F-86F Sabre", 16, 100.0),
        ("MiG-21Bis", 50, 100.0),
        ("MiG-15bis", 42, 100.0),
        ("L-39C", 84, 100.0),
        ("L-39ZA", 84, 100.0),
        ("F-5E", 16, 100.0),
        ("F-5E-3", 16, 100.0),
        ("AJS37", 139, 100.0),
    ] {
        let (lua, frame, _) = loaded(true);
        lua.globals().set("aircraft_name", aircraft).unwrap();
        frame.call::<()>(()).unwrap();
        let packets: Table = lua.globals().get("packets").unwrap();
        let packet: Table = packets.get(1).unwrap();
        let text: String = packet.get(1).unwrap();
        assert_eq!(text.split(',').nth(1).unwrap(), format!("{:.2}", 0.625 * factor), "{aircraft}");
        let calls: Table = lua.globals().get("calls").unwrap();
        assert_eq!(calls.get::<String>(calls.raw_len()).unwrap(), format!("gauge:{index}"));
    }
}

#[test]
fn failed_sdk_sample_sends_nothing_and_keeps_the_completed_call_prefix() {
    for hook in [false, true] {
        let (lua, frame, _) = loaded(hook);
        lua.load("local f = function() note('aircraft'); return nil end; Export.LoGetSelfData = f; LoGetSelfData = f")
            .exec()
            .unwrap();
        assert!(frame.call::<()>(()).is_err());
        assert_eq!(lua.globals().get::<Table>("packets").unwrap().raw_len(), 0);
        let calls: Table = lua.globals().get("calls").unwrap();
        let calls: Vec<String> = calls.sequence_values().collect::<mlua::Result<_>>().unwrap();
        assert_eq!(calls, ["engine", "indicated", "true_speed", "acceleration", "attitude", "angular", "aircraft"]);
    }
}

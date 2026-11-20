-- SPDX-License-Identifier: GPL-3.0-or-later
-- OpenRigCue telemetry output for DCS World.

local function send_frame()
    local engine = LoGetEngineInfo()
    local indicated_speed = LoGetIndicatedAirSpeed()
    local velocity = LoGetTrueAirSpeed()
    local acceleration = LoGetAccelerationUnits()
    local pitch, bank, yaw = LoGetADIPitchBankYaw()
    local angular = LoGetAngularVelocity()
    local aircraft = LoGetSelfData()
    local aircraft_name = aircraft.Name
    local mechanisms = LoGetMechInfo()
    local snares = LoGetSnares()
    local payload = LoGetPayloadInfo()

    local canopy, gear, shells = 0, 0, 0
    if mechanisms ~= nil then
        canopy = mechanisms.canopy.value
        gear = mechanisms.gear.value
    end
    if payload ~= nil then
        shells = payload.Cannon.shells
    end
    local ax, ay, az = 0, 0, 0
    if acceleration ~= nil then
        ay = acceleration.y
        az = acceleration.z
        ax = acceleration.x
    end
    local rpm = 0
    if engine ~= nil then
        rpm = engine.RPM.left
    end
    local packet = string.format("%.2f,%.2f,%.2f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%s,%s,%s,%.5f",
        1, rpm, velocity, ax, ay, az, pitch, bank, yaw,
        angular.z, angular.x, angular.y, aircraft_name, canopy, gear, shells)
    socket.try(c:sendto(packet, host, port))
end

function LuaExportStart()
    package.path = package.path .. ";" .. lfs.currentdir() .. "/LuaSocket/?.lua"
    package.cpath = package.cpath .. ";" .. lfs.currentdir() .. "/LuaSocket/?.dll"
    socket = require("socket")
    host = "127.0.0.1"
    port = tonumber("^port^")
    c = socket.udp()
end
function LuaExportBeforeNextFrame() end
LuaExportAfterNextFrame = send_frame
function LuaExportStop()
    c:close()
end
function LuaExportActivityNextEvent(time)
    return time
end

-- SPDX-License-Identifier: GPL-3.0-or-later
-- OpenRigCue telemetry output for DCS World.

local gauges = {
    ["Mi-8MT"] = {42,100}, ["A-10C"] = {78,100}, ["UH-1H"] = {123,100},
    ["Ka-50"] = {52,100}, ["SA342M"] = {52,100}, ["SA342L"] = {52,100},
    ["SA342Mistral"] = {52,100}, ["SA342Minigun"] = {52,100},
    ["P-51D"] = {23,151}, ["P-51D-30-NA"] = {23,151}, ["TF-51D"] = {23,151},
    ["FW-190D9"] = {47,1}, ["Bf-109K-4"] = {29,1},
    ["SpitfireLFMkIX"] = {37,1}, ["SpitfireLFMkIXCW"] = {37,1},
    ["F-86F Sabre"] = {16,100}, ["MiG-21Bis"] = {50,100}, ["MiG-15bis"] = {42,100},
    ["L-39C"] = {84,100}, ["L-39ZA"] = {84,100}, ["F-5E"] = {16,100},
    ["F-5E-3"] = {16,100}, ["AJS37"] = {139,100}
}

local function send_frame()
    local engine = Export.LoGetEngineInfo()
    local indicated_speed = Export.LoGetIndicatedAirSpeed()
    local velocity = Export.LoGetTrueAirSpeed()
    local acceleration = Export.LoGetAccelerationUnits()
    local pitch, bank, yaw = Export.LoGetADIPitchBankYaw()
    local angular = Export.LoGetAngularVelocity()
    local aircraft = Export.LoGetSelfData()
    local aircraft_name = aircraft.Name
    local mechanisms = Export.LoGetMechInfo()
    local snares = Export.LoGetSnares()
    local payload = Export.LoGetPayloadInfo()
    local panel = Export.GetDevice(0)
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
    local gauge = gauges[aircraft_name]
    if gauge then
        rpm = panel:get_argument_value(gauge[1]) * gauge[2]
    elseif engine ~= nil then
        rpm = engine.RPM.left
    end
    local packet = string.format("%.2f,%.2f,%.2f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%.5f,%s,%s,%s,%.5f",
        1, rpm, velocity, ax, ay, az, pitch, bank, yaw,
        angular.z, angular.x, angular.y, aircraft_name, canopy, gear, shells)
    socket.try(c:sendto(packet, host, port))
end

local callbacks = {}
function callbacks.onSimulationStart()
    package.path = package.path .. ";" .. lfs.currentdir() .. "/LuaSocket/?.lua"
    package.cpath = package.cpath .. ";" .. lfs.currentdir() .. "/LuaSocket/?.dll"
    socket = require("socket")
    host = "127.0.0.1"
    port = tonumber("^port^")
    c = socket.udp()
end
callbacks.onSimulationFrame = send_frame
function callbacks.onSimulationStop()
    c:close()
end
DCS.setUserCallbacks(callbacks)

-- SPDX-License-Identifier: GPL-3.0-or-later
-- OpenRigCue Farming Simulator telemetry bridge.

TelemetryMod = {}
TelemetryMod.Context = {}
TelemetryMod.Context.isLoaded = false
TelemetryMod.Context.updateCount = 0
TelemetryMod.Context.pipeName = "\\\\.\\pipe\\FMTelemetry"
TelemetryMod.Context.tabdata = {}
TelemetryMod.Context.tabLength = 1
TelemetryMod.Context.shfile = nil

function TelemetryMod:GetCurrentVehicle()
    return TelemetryMod:GetCurrentPlayer().getCurrentVehicle()
end

function TelemetryMod:GetCurrentPlayer()
    return g_currentMission.playerSystem.playersByUserId[g_currentMission.playerUserId]
end

function TelemetryMod:logError(message)
    Logging.error("[TelemetryMod] %s", message)
end

function TelemetryMod:logInfo(message)
    Logging.info("[TelemetryMod] %s", message)
end

function TelemetryMod:update(dt)
    self:createPipe(dt)

    if TelemetryMod.Context.shfile ~= nil then
        self:buildTelemetry()
    end
end

function TelemetryMod:createPipe(dt)
    if TelemetryMod.Context.updateCount == 0 then
        if TelemetryMod.Context.shfile ~= nil then
            TelemetryMod.Context.shfile:flush()
            TelemetryMod.Context.shfile:close()
        end

        local handle = io.open(TelemetryMod.Context.pipeName, "w")
        TelemetryMod.Context.shfile = handle
    end

    TelemetryMod.Context.updateCount = TelemetryMod.Context.updateCount + 1
    if TelemetryMod.Context.updateCount == 256 then
        TelemetryMod.Context.updateCount = 0
    end
end

function TelemetryMod:buildTelemetry()
    TelemetryMod.Context.tabdata = {}
    TelemetryMod.Context.tabLength = 1
    self:addRawStringToTelemetry("{")

    local vehicle = TelemetryMod:GetCurrentVehicle()

    if vehicle ~= nil then
        if not vehicle.isBroken then
            for index, component in ipairs(vehicle.components) do
                local node = component.node

                local avx, avy, avz = getAngularVelocity(node)
                local lvx, lvy, lvz = getLinearVelocity(node)
                local wx, wy, wz = getWorldTranslation(node)
                local wrx, wry, wrz = getWorldRotation(node)
                local tx, ty, tz = getTranslation(node)
                local rx, ry, rz = getRotation(node)
                local qx, qy, qz, qw = getQuaternion(node)

                self:addFloatToTelemetry("avX" .. index, avx)
                self:addFloatToTelemetry("avY" .. index, avy)
                self:addFloatToTelemetry("avZ" .. index, avz)

                self:addFloatToTelemetry("lvX" .. index, lvx)
                self:addFloatToTelemetry("lvY" .. index, lvy)
                self:addFloatToTelemetry("lvZ" .. index, lvz)

                self:addFloatToTelemetry("transWorldX" .. index, wx)
                self:addFloatToTelemetry("transWorldY" .. index, wy)
                self:addFloatToTelemetry("transWorldZ" .. index, wz)

                self:addFloatToTelemetry("rotWorldX" .. index, wrx)
                self:addFloatToTelemetry("rotWorldY" .. index, wry)
                self:addFloatToTelemetry("rotWorldZ" .. index, wrz)

                self:addFloatToTelemetry("transX" .. index, tx)
                self:addFloatToTelemetry("transY" .. index, ty)
                self:addFloatToTelemetry("transZ" .. index, tz)

                self:addFloatToTelemetry("rotX" .. index, rx)
                self:addFloatToTelemetry("rotY" .. index, ry)
                self:addFloatToTelemetry("rotZ" .. index, rz)

                self:addFloatToTelemetry("qx" .. index, qx)
                self:addFloatToTelemetry("qy" .. index, qy)
                self:addFloatToTelemetry("qz" .. index, qz)
                self:addFloatToTelemetry("qw" .. index, qw)
            end

            local motor = vehicle:getMotor()
            local rpm = motor:getLastMotorRpm()
            local rpmReal = motor:getLastRealMotorRpm()
            local speed = vehicle:getLastSpeed()

            self:addFloatToTelemetry("rpm", rpm)
            self:addFloatToTelemetry("rpmReal", rpmReal)
            self:addFloatToTelemetry("speed", speed)
        end
    else
        self:addFloatToTelemetry("avX1", 0)
        self:addFloatToTelemetry("avY1", 0)
        self:addFloatToTelemetry("avZ1", 0)
        self:addFloatToTelemetry("avX2", 0)
        self:addFloatToTelemetry("avY2", 0)
        self:addFloatToTelemetry("avZ2", 0)
        self:addFloatToTelemetry("lvX1", 0)
        self:addFloatToTelemetry("lvY1", 0)
        self:addFloatToTelemetry("lvZ1", 0)
        self:addFloatToTelemetry("lvX2", 0)
        self:addFloatToTelemetry("lvY2", 0)
        self:addFloatToTelemetry("lvZ2", 0)
        self:addFloatToTelemetry("transWorldX1", 0)
        self:addFloatToTelemetry("transWorldY1", 0)
        self:addFloatToTelemetry("transWorldZ1", 0)
        self:addFloatToTelemetry("transWorldX2", 0)
        self:addFloatToTelemetry("transWorldY2", 0)
        self:addFloatToTelemetry("transWorldZ2", 0)
        self:addFloatToTelemetry("rotWorldX1", 0)
        self:addFloatToTelemetry("rotWorldY1", 0)
        self:addFloatToTelemetry("rotWorldZ1", 0)
        self:addFloatToTelemetry("rotWorldX2", 0)
        self:addFloatToTelemetry("rotWorldY2", 0)
        self:addFloatToTelemetry("rotWorldZ2", 0)
        self:addFloatToTelemetry("transX1", 0)
        self:addFloatToTelemetry("transY1", 0)
        self:addFloatToTelemetry("transZ1", 0)
        self:addFloatToTelemetry("transX2", 0)
        self:addFloatToTelemetry("transY2", 0)
        self:addFloatToTelemetry("transZ2", 0)
        self:addFloatToTelemetry("rotX1", 0)
        self:addFloatToTelemetry("rotY1", 0)
        self:addFloatToTelemetry("rotZ1", 0)
        self:addFloatToTelemetry("rotX2", 0)
        self:addFloatToTelemetry("rotY2", 0)
        self:addFloatToTelemetry("rotZ2", 0)
        self:addFloatToTelemetry("rpm", 0)
        self:addFloatToTelemetry("rpmReal", 0)
        self:addFloatToTelemetry("speed", 0)
        self:addFloatToTelemetry("qx1", 0)
        self:addFloatToTelemetry("qy1", 0)
        self:addFloatToTelemetry("qz1", 0)
        self:addFloatToTelemetry("qw1", 0)
        self:addFloatToTelemetry("qx2", 0)
        self:addFloatToTelemetry("qy2", 0)
        self:addFloatToTelemetry("qz2", 0)
        self:addFloatToTelemetry("qw2", 0)
    end

    self:addRawStringToTelemetry("}")
    local frame = table.concat(TelemetryMod.Context.tabdata)
    TelemetryMod.Context.shfile:write(frame)
    TelemetryMod.Context.shfile:flush()
end

function TelemetryMod:addFloatToTelemetry(name, value)
    if value ~= nil then
        TelemetryMod.Context.tabdata[TelemetryMod.Context.tabLength] = string.format('"%s": %.4f, ', name, value)
        self:incrementTablePosition()
    end
end

function TelemetryMod:addRawStringToTelemetry(value)
    TelemetryMod.Context.tabdata[TelemetryMod.Context.tabLength] = value
    self:incrementTablePosition()
end

function TelemetryMod:incrementTablePosition()
    TelemetryMod.Context.tabLength = TelemetryMod.Context.tabLength + 1
end

addModEventListener(TelemetryMod)

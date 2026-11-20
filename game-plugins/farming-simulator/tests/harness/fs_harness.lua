-- Behavioural trace harness for the Farming Simulator TelemetryMod (Lua 5.1).
--
-- Loads a TelemetryMod.lua script into a mocked GIANTS-engine
-- environment in which every global read/write, every field read on a game
-- object, every engine call (arguments and results) and every pipe write is
-- recorded.  The returned trace is the script's observable behaviour.
--
-- Usage:  local run = dofile("fs_harness.lua")
--         local text = run(source, edition, scenario, updates, logged)
--   edition  : "fs19" (FS19/FS22 API: g_currentMission.controlledVehicle)
--              "fs25" (FS25 API: g_currentMission.playerSystem...)
--   scenario : see SCENARIOS below
--   updates  : number of update(dt) calls
--   logged   : optional set {[n]=true} of update numbers whose trace is kept
--              (the others still run, silently)
--   raw      : optional; when true nothing is filtered or masked: reads of
--              base-library globals, load-time table.concat calls and
--              string.format calls with the "%s:%s: %s" error pattern are
--              traced, and runtime/load errors are logged with their
--              message.
--
-- Unless `raw` is set, reads of the Lua base-library globals listed in
-- INTERNAL and table.concat calls made while the script loads are left out
-- of the trace.  Engine globals, game-object fields, engine calls and pipe
-- writes are always traced.

local INTERNAL = {
  unpack = 1, select = 1, setmetatable = 1, getfenv = 1, setfenv = 1, tonumber = 1,
  assert = 1, pcall = 1, error = 1, type = 1, tostring = 1, pairs = 1, next = 1,
  rawget = 1, rawset = 1, string = 1, table = 1, math = 1, bit = 1, debug = 1,
  _ENV = 1, _G = 1,
}

local SCENARIOS = {
  novehicle = true,      -- no controlled vehicle, player present
  noplayer = true,       -- no controlled vehicle, no player
  veh0 = true,           -- vehicle with zero components
  veh1 = true, veh2 = true, veh3 = true,
  veh2noplayer = true,   -- vehicle but player field nil (fs19 only meaningful)
  broken = true,         -- vehicle.isBroken == true
  nilquat = true,        -- getQuaternion returns only 3 values (qw nil)
  nomotor = true,        -- vehicle:getMotor() returns nil -> runtime error
  openfail = true,       -- io.open always fails
  openfail_once = true,  -- first io.open fails, later ones succeed
  noplayersys = true,    -- fs25: player lookup yields nil -> runtime error
}

return function(source, edition, scenario, updates, logged, raw)
  assert(SCENARIOS[scenario], "unknown scenario " .. tostring(scenario))
  local out = {}
  local quiet = false
  local loading = true
  local function log(s) if not quiet then out[#out + 1] = s end end

  local names = setmetatable({}, { __mode = "k" })
  local function fmt(v)
    local t = type(v)
    if t == "string" then
      if #v > 4000 then return "<string len " .. #v .. ">" end
      return string.format("%q", v)
    elseif t == "number" then
      return string.format("%.17g", v)
    elseif t == "table" or t == "function" then
      return names[v] or t
    end
    return tostring(v)
  end
  local function fmtargs(...)
    local t = {}
    for i = 1, select("#", ...) do t[#t + 1] = fmt((select(i, ...))) end
    return table.concat(t, ", ")
  end
  local function proxy(name, backing)
    local p = {}
    names[p] = name
    return setmetatable(p, {
      __index = function(_, k) local v = backing[k]; log("GET " .. name .. "." .. tostring(k) .. " -> " .. fmt(v)); return v end,
      __newindex = function(_, k, v) log("SET " .. name .. "." .. tostring(k) .. " = " .. fmt(v)); backing[k] = v end,
    })
  end
  local function fn(name, f)
    local w = function(...)
      local r = { f(...) }
      log("CALL " .. name .. "(" .. fmtargs(...) .. ") => " .. fmtargs(unpack(r, 1, table.maxn(r))))
      return unpack(r, 1, table.maxn(r))
    end
    names[w] = "fn:" .. name
    return w
  end

  -- named pipe (io.open result)
  local nfiles, nopen = 0, 0
  local function mkfile()
    nfiles = nfiles + 1
    local id = "file" .. nfiles
    local f = {}
    local p = proxy(id, f)
    f.write = fn(id .. ":write", function(self, s) log("WRITE " .. id .. " <<" .. tostring(s) .. ">>"); return self end)
    f.flush = fn(id .. ":flush", function() return true end)
    f.close = fn(id .. ":close", function() return true end)
    return p
  end
  local mio = {
    open = fn("io.open", function(path, mode)
      nopen = nopen + 1
      if scenario == "openfail" or (scenario == "openfail_once" and nopen == 1) then
        return nil, path .. ": No such file or directory", 2
      end
      return mkfile()
    end),
  }

  -- physics: every call returns fresh, distinct values
  local counter = 0
  local function vals(n)
    local t = {}
    for i = 1, n do
      counter = counter + 1
      local v = counter * 1.0001 - 0.37 * (counter % 3)
      if counter % 4 == 0 then v = -v end
      t[i] = v
    end
    return unpack(t, 1, n)
  end
  local engine = {}
  for _, nm in ipairs({ "getAngularVelocity", "getLinearVelocity", "getWorldTranslation",
                        "getWorldRotation", "getTranslation", "getRotation" }) do
    engine[nm] = fn(nm, function(node) return vals(3) end)
  end
  engine.getQuaternion = fn("getQuaternion", function(node)
    if scenario == "nilquat" then return vals(3) end
    return vals(4)
  end)

  local motor = proxy("motor", {
    getLastMotorRpm = fn("motor:getLastMotorRpm", function() return 1234.56789 end),
    getLastRealMotorRpm = fn("motor:getLastRealMotorRpm", function() return -0.00004 end),
  })
  local function mkvehicle(ncomp, broken)
    local comps = {}
    for i = 1, ncomp do comps[i] = proxy("component" .. i, { node = 1000 + i }) end
    names[comps] = "componentsArray"
    return proxy("vehicle", {
      isBroken = broken,
      components = comps, -- plain array: ipairs() in Lua 5.1 reads it raw
      getMotor = fn("vehicle:getMotor", function()
        if scenario == "nomotor" then return nil end
        return motor
      end),
      getLastSpeed = fn("vehicle:getLastSpeed", function() return 23.45675 end),
    })
  end

  local vehicle, playerPresent = nil, true
  if scenario == "veh0" then vehicle = mkvehicle(0, false)
  elseif scenario == "veh1" then vehicle = mkvehicle(1, false)
  elseif scenario == "veh2" or scenario == "nilquat" or scenario == "nomotor" then vehicle = mkvehicle(2, false)
  elseif scenario == "veh3" then vehicle = mkvehicle(3, false)
  elseif scenario == "veh2noplayer" then vehicle = mkvehicle(2, false); playerPresent = false
  elseif scenario == "broken" then vehicle = mkvehicle(2, true)
  elseif scenario == "noplayer" then playerPresent = false
  end

  local mission = {}
  if edition == "fs19" then
    mission.controlledVehicle = vehicle
    mission.player = playerPresent and proxy("player", {}) or nil
  elseif edition == "fs25" then
    local player = proxy("player", {
      getCurrentVehicle = fn("player.getCurrentVehicle", function() return vehicle end),
    })
    local byId = {}
    if scenario ~= "noplayersys" then byId[7] = player end
    mission.playerUserId = 7
    mission.playerSystem = proxy("playerSystem", { playersByUserId = proxy("playersByUserId", byId) })
  else
    error("unknown edition " .. tostring(edition))
  end

  local captured
  local env = {}
  for k, v in pairs(_G) do env[k] = v end
  env.io = proxy("io", mio)
  env.g_currentMission = proxy("g_currentMission", mission)
  env.addModEventListener = fn("addModEventListener", function(m) captured = m end)
  env.Logging = proxy("Logging", {
    error = fn("Logging.error", function() end),
    info = fn("Logging.info", function() end),
  })
  for k, v in pairs(engine) do env[k] = v end
  env.ipairs = fn("ipairs", ipairs)
  env.string = {}
  for k, v in pairs(string) do env.string[k] = v end
  -- string.format("%s:%s: %s", chunk, line, msg) is the shape of an error
  -- decorator, not telemetry formatting; it is passed through untraced unless
  -- `raw` is set (error text is masked as "RUNTIME ERROR" anyway).
  local tracedFormat = fn("string.format", string.format)
  env.string.format = function(f, ...)
    if f == "%s:%s: %s" and not raw then return string.format(f, ...) end
    return tracedFormat(f, ...)
  end
  env.table = {}
  for k, v in pairs(table) do env.table[k] = v end
  -- table.concat calls made while the script loads are traced only in raw
  -- mode; calls made after loading are always recorded.
  env.table.concat = function(...)
    local r = table.concat(...)
    if raw or not loading then log("CALL table.concat(" .. fmtargs(...) .. ") => " .. fmt(r)) end
    return r
  end
  names[env.table.concat] = "fn:table.concat"

  local genv = setmetatable({}, {
    __index = function(_, k)
      local v = env[k]
      if raw or not INTERNAL[k] then log("GGET " .. tostring(k) .. " -> " .. fmt(v)) end
      return v
    end,
    __newindex = function(_, k, v)
      if type(v) == "table" and names[v] == nil then names[v] = "table:" .. tostring(k) end
      log("GSET " .. tostring(k) .. " = " .. fmt(v))
      env[k] = v
    end,
  })

  local chunk = assert(loadstring(source, "=TelemetryMod.lua"))
  setfenv(chunk, genv)
  local oldenv = getfenv(0)
  setfenv(0, genv)
  log("### LOAD")
  local ok, err = pcall(chunk)
  if not ok then log(raw and ("LOAD ERROR " .. tostring(err)) or "LOAD ERROR") end
  loading = false
  if captured then
    names[captured] = names[captured] or "modTable"
    local ks = {}
    for k, v in pairs(captured) do ks[#ks + 1] = tostring(k) .. "=" .. type(v) end
    table.sort(ks)
    log("### mod table: " .. table.concat(ks, " "))
    local ctx = rawget(captured, "Context")
    if type(ctx) == "table" then
      local cs = {}
      for k, v in pairs(ctx) do cs[#cs + 1] = tostring(k) .. "=" .. fmt(v) end
      table.sort(cs)
      log("### Context: " .. table.concat(cs, " "))
    end
    for i = 1, updates do
      quiet = logged ~= nil and not logged[i]
      log("### update " .. i)
      local ok2, err2 = pcall(captured.update, captured, 16.666)
      if not ok2 then log(raw and ("RUNTIME ERROR " .. tostring(err2)) or "RUNTIME ERROR") end
    end
    quiet = false
    -- Direct calls of every mod-table function (sorted by name), so helpers
    -- the engine never calls (FS25 logError/logInfo, ...) are compared too.
    local fnames = {}
    for k, v in pairs(captured) do if type(v) == "function" and k ~= "update" then fnames[#fnames + 1] = k end end
    table.sort(fnames)
    for _, k in ipairs(fnames) do
      log("### direct " .. k)
      local ok3, err3 = pcall(captured[k], captured, "probe", 2.5)
      if not ok3 then log(raw and ("RUNTIME ERROR " .. tostring(err3)) or "RUNTIME ERROR") end
    end
  else
    log("### no mod registered")
  end
  setfenv(0, oldenv)
  return table.concat(out, "\n") .. "\n"
end

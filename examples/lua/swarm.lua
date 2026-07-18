-- Example minerider-lua swarm script: 2 SOCKS5 proxy groups of 3 bots
-- each, connected to one local server, with lifecycle/chat/GUI handlers.
--
-- Run it (against your own local server — never a public one):
--   MINERIDER_PROXY1_USER=alice MINERIDER_PROXY1_PASS=hunter2 \
--   MINERIDER_PROXY2_USER=bob   MINERIDER_PROXY2_PASS=hunter3 \
--   cargo run --release --features lua --bin minerider-lua -- examples/lua/swarm.lua
--
-- Every host/port/env-var-name below is a local placeholder. Proxy
-- credentials are never written in this script or read directly by Lua —
-- only the *names* of the environment variables that hold them are given
-- to swarm:add_proxy; Rust resolves the actual values outside the sandbox.
-- See docs/lua_wrapper.md#proxies for the full security model.

-- ---------------------------------------------------------------------
-- Configuration phase: runs exactly once, on the coordinator worker.
-- ---------------------------------------------------------------------
swarm:configure(function()
    swarm:add_server({
        name = "local",
        host = "127.0.0.1",
        port = 25565,
        view_distance = 10,
        shared_chunks = true,
    })

    swarm:add_proxy({
        name = "proxy1",
        host = "127.0.0.1",
        port = 1080,
        username_env = "MINERIDER_PROXY1_USER",
        password_env = "MINERIDER_PROXY1_PASS",
    })
    swarm:add_proxy({
        name = "proxy2",
        host = "127.0.0.1",
        port = 1081,
        username_env = "MINERIDER_PROXY2_USER",
        password_env = "MINERIDER_PROXY2_PASS",
    })

    local reconnect = {
        enabled = true,
        max_retries = 10,
        initial_delay_ms = 1000,
        max_delay_ms = 30000,
        multiplier = 2.0,
        jitter = { type = "deterministic", fraction = 0.2 },
        stable_session_reset_ms = 60000,
        on_transient = "retry",
        on_server_rejected = "stop",
        on_auth_failure = "stop",
        on_protocol_incompatible = "stop",
    }

    swarm:add_group({
        name = "group1",
        count = 3,
        id_prefix = "g1-",
        username_prefix = "Swarm1_",
        server = "local",
        proxy = "proxy1",
        reconnect = reconnect,
    })
    swarm:add_group({
        name = "group2",
        count = 3,
        id_prefix = "g2-",
        username_prefix = "Swarm2_",
        server = "local",
        proxy = "proxy2",
        reconnect = reconnect,
    })
end)

-- ---------------------------------------------------------------------
-- Handler registration: runs independently in every worker. Each worker
-- only ever receives events for the bots assigned to it (bot_id % worker
-- count), so these handlers naturally only fire for that worker's share.
-- ---------------------------------------------------------------------
swarm:on("connected", function(bot, event)
    minerider.log("info", bot:username() .. " connected (worker " .. bot:worker_id() .. ")")
end)

swarm:on("disconnected", function(bot, event)
    minerider.log("warn", bot:username() .. " disconnected: " .. event.reason)
end)

swarm:on("reconnect_scheduled", function(bot, event)
    minerider.log("info", bot:username() .. " reconnecting (attempt " .. event.attempt .. ")")
end)

swarm:on("chat", function(bot, event)
    minerider.log("info", "<" .. event.sender .. "> " .. event.message)
    if event.message == "!hello" then
        bot:chat("hello from " .. bot:username())
    end
end)

swarm:on("gui_opened", function(bot, event)
    minerider.log("info", bot:username() .. " opened window " .. event.window_id)
    -- Inspect the first slot; click it if it holds an item.
    local view = bot:open_gui()
    if view and view.slots[1] and not view.slots[1].empty then
        bot:click_gui(view.slots[1].raw_slot, "left", function(result)
            minerider.log("info", bot:username() .. " click result: " .. result.outcome)
        end)
    end
end)

swarm:on("action_result", function(bot, event)
    if not event.ok then
        minerider.log("warn", bot:username() .. " action " .. event.request_id .. " failed: " .. event.error.code)
    end
end)

-- ---------------------------------------------------------------------
-- Start: finalizes configuration, connects every bot, and hands control
-- to each worker's persistent dispatch loop.
-- ---------------------------------------------------------------------
swarm:connect_all()
swarm:run()

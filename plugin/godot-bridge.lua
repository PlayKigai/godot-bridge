local bridge = require("godot-bridge")

local function command(name, desc, run, nargs)
  vim.api.nvim_create_user_command(name, run, { desc = desc, nargs = nargs or 0 })
end

command("GodotRun", "Run the Godot project", bridge.run)
command("GodotRunScene", "Run the scene of the current file", bridge.run_scene)
command("GodotEditor", "Open the Godot editor", bridge.open_editor)
command("GodotDoc", "Open the Godot class reference for a symbol", function(args)
  bridge.doc(args.args)
end, "?")
command("GodotStatus", "Show godot-bridge status", bridge.status)
command("GodotRestart", "Restart the Godot language server", bridge.restart)

local M = {}

local VERSION = "1.0.0"
local INSTALL_HINT = "install a matching godot-bridge: cargo install --git https://github.com/PlayKigai/godot-bridge godot-bridge --locked"

local config = { bridge_path = "godot-bridge", settings = {} }
local resolved_bridge

local function is_absolute(path)
  return path:match("^[/\\]") ~= nil or path:match("^%a:[/\\]") ~= nil
end

local function on_path(name)
  local windows = vim.uv.os_uname().sysname == "Windows_NT"
  local extensions = windows and vim.list_extend({ "" }, vim.split(vim.env.PATHEXT or ".EXE", ";")) or { "" }
  for _, entry in ipairs(vim.split(vim.env.PATH or "", windows and ";" or ":")) do
    local dir = entry:gsub('^"(.*)"$', "%1")
    for _, extension in ipairs(extensions) do
      local stat = is_absolute(dir) and vim.uv.fs_stat(vim.fs.joinpath(dir, name .. extension))
      if stat and stat.type == "file" then
        return vim.fs.joinpath(dir, name .. extension)
      end
    end
  end
end

-- Bare and relative names are resolved here because Windows would search the project cwd first.
local function bridge()
  if resolved_bridge then
    return resolved_bridge
  end
  local name = type(config.bridge_path) == "string" and vim.trim(config.bridge_path) or ""
  if name == "" then
    name = "godot-bridge"
  end
  if name:match("^~[/\\]") then
    name = vim.fs.joinpath(vim.uv.os_homedir(), name:sub(3))
  end
  if name == vim.fs.basename(name) then
    local found = on_path(name)
    if not found then
      return vim.fs.joinpath(vim.uv.os_homedir(), ".cargo", "bin", name)
    end
    name = found
  elseif not is_absolute(name) then
    name = vim.fs.joinpath(vim.uv.os_homedir(), name)
  end
  resolved_bridge = name
  return name
end

local function bridge_env()
  local settings = type(config.settings) == "table" and next(config.settings) ~= nil and config.settings or vim.empty_dict()
  return { GODOT_BRIDGE_SETTINGS = vim.json.encode(settings) }
end

local function root_of(bufnr)
  local file = vim.api.nvim_buf_get_name(bufnr or 0)
  if file == "" then
    return nil
  end
  local marker = vim.fs.find("project.godot", { path = vim.fs.dirname(file), upward = true })[1]
  if not marker then
    vim.notify("godot-bridge: no project.godot above " .. file, vim.log.levels.WARN)
    return nil
  end
  return vim.fs.dirname(marker)
end

local function notify_result(label, result)
  local text = vim.trim((result.stdout or "") .. (result.stderr or ""))
  local level = result.code == 0 and vim.log.levels.INFO or vim.log.levels.ERROR
  vim.notify(("godot-bridge %s: exit %d\n%s"):format(label, result.code, text), level)
end

local function run_in_root(args)
  local root = root_of()
  if not root then
    return
  end
  vim.system(
    vim.list_extend({ bridge() }, args),
    { cwd = root, env = bridge_env() },
    vim.schedule_wrap(function(result)
      notify_result(args[1], result)
    end)
  )
end

local function check_version()
  vim.system({ bridge(), "--version" }, { timeout = 5000 }, vim.schedule_wrap(function(result)
    local bridge_version = result.code == 0 and (result.stdout or ""):match("godot%-bridge (%d+%.%d+)")
    if not bridge_version then
      vim.notify("godot-bridge: `" .. bridge() .. " --version` failed; " .. INSTALL_HINT, vim.log.levels.WARN)
    elseif bridge_version ~= VERSION:match("%d+%.%d+") then
      vim.notify(("godot-bridge: plugin %s, bridge %s; %s"):format(VERSION, bridge_version, INSTALL_HINT), vim.log.levels.WARN)
    end
  end))
end

local function start_lsp(bufnr)
  local root = root_of(bufnr)
  if not root then
    return
  end
  vim.lsp.start({
    name = "godot",
    cmd = { bridge(), "lsp" },
    cmd_cwd = root,
    cmd_env = bridge_env(),
    root_dir = root,
  }, { bufnr = bufnr })
end

function M.setup(opts)
  config = vim.tbl_extend("force", config, type(opts) == "table" and opts or {})
  resolved_bridge = nil
  if vim.uv.os_uname().sysname == "Darwin" then
    vim.notify("godot-bridge: macOS is not supported", vim.log.levels.ERROR)
    return
  end
  check_version()
  vim.api.nvim_create_autocmd("FileType", {
    group = vim.api.nvim_create_augroup("godot-bridge", { clear = true }),
    pattern = "gdscript",
    callback = function(args)
      start_lsp(args.buf)
    end,
  })
end

function M.run()
  run_in_root({ "run", "--file", vim.api.nvim_buf_get_name(0) })
end

function M.run_scene()
  run_in_root({ "run", "--file", vim.api.nvim_buf_get_name(0), "--scene", "current" })
end

function M.open_editor()
  run_in_root({ "open-editor", "--file", vim.api.nvim_buf_get_name(0) })
end

function M.doc(symbol)
  if not symbol or symbol == "" then
    symbol = vim.fn.expand("<cWORD>"):match("[A-Za-z_][A-Za-z0-9_.]*")
  end
  if symbol then
    run_in_root({ "doc", symbol })
  else
    vim.notify("godot-bridge: no symbol under the cursor", vim.log.levels.WARN)
  end
end

function M.status()
  vim.system({ bridge(), "status" }, { timeout = 5000 }, vim.schedule_wrap(function(result)
    notify_result("status", result)
  end))
end

function M.restart()
  local bufnr = vim.api.nvim_get_current_buf()
  for _, client in ipairs(vim.lsp.get_clients({ bufnr = bufnr, name = "godot" })) do
    client:stop()
  end
  local stopped = vim.wait(2000, function()
    return #vim.lsp.get_clients({ bufnr = bufnr, name = "godot" }) == 0
  end)
  if stopped then
    start_lsp(bufnr)
  else
    vim.notify("godot-bridge: the language server did not stop", vim.log.levels.ERROR)
  end
end

function M.dap()
  local ok, dap = pcall(require, "dap")
  if not ok then
    vim.notify("godot-bridge: nvim-dap is not installed", vim.log.levels.ERROR)
    return
  end
  dap.adapters.godot = function(callback)
    local file = vim.api.nvim_buf_get_name(0)
    callback({
      type = "executable",
      command = bridge(),
      args = file ~= "" and { "dap", "--file", file } or { "dap" },
      options = { cwd = root_of(), env = bridge_env() },
    })
  end
  dap.configurations.gdscript = dap.configurations.gdscript or {
    { type = "godot", request = "launch", name = "Godot: run project", scene = "main" },
    { type = "godot", request = "launch", name = "Godot: run current scene", scene = "current" },
    { type = "godot", request = "attach", name = "Godot: attach" },
  }
end

return M

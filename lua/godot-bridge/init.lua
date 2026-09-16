local M = {}

local VERSION = "1.0.6"
local CARGO_LINE = "cargo install godot-bridge --locked"
local INSTALL_HINT = "run :GodotBridgeInstall or " .. CARGO_LINE
local MACOS_MSG = "godot-bridge: macOS is not supported. Linux and Windows only."
local RELEASE_BASE = "https://github.com/PlayKigai/godot-bridge/releases/download"

local config = { bridge_path = "godot-bridge", settings = {} }
local resolved_bridge
local installing = false

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

local function storage_dir()
  return vim.fs.joinpath(vim.fn.stdpath("data"), "godot-bridge")
end

local function asset_name()
  local uname = vim.uv.os_uname()
  local sysname = uname.sysname
  local raw
  if sysname == "Linux" then
    raw = uname.machine
  elseif sysname == "Windows_NT" then
    raw = vim.env.PROCESSOR_ARCHITECTURE
  else
    return nil
  end
  local arch = (raw or ""):lower()
  local norm
  if arch == "x86_64" or arch == "amd64" or arch == "x64" then
    norm = "x86_64"
  elseif arch == "aarch64" or arch == "arm64" then
    norm = "aarch64"
  else
    return nil
  end
  if sysname == "Linux" then
    return ("godot-bridge-v%s-%s-linux"):format(VERSION, norm)
  end
  return ("godot-bridge-v%s-%s-windows.exe"):format(VERSION, norm)
end

local function cargo_bin(name)
  local home = vim.env.CARGO_HOME
  if type(home) == "string" and home ~= "" then
    return vim.fs.joinpath(home, "bin", name)
  end
  return vim.fs.joinpath(vim.uv.os_homedir(), ".cargo", "bin", name)
end

local function sums_hash(contents, asset)
  local found
  local count = 0
  for line in (contents .. "\n"):gmatch("([^\n]*)\n") do
    line = line:gsub("\r$", "")
    local hash, sep = line:sub(1, 64), line:sub(65, 66)
    if #hash == 64 and hash:match("^%x+$") and (sep == "  " or sep == " *") and line:sub(67) == asset then
      found = hash
      count = count + 1
    end
  end
  return found, count
end

-- vim.fn.sha256 is not NUL-safe, so hash tools run externally.
local function first_hash(text)
  local compact = (text or ""):gsub("%s+", "")
  local hash = compact:match(("%x"):rep(64))
  return hash and hash:lower() or nil
end

-- Bare names are resolved here because Windows would search the project cwd first.
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
  if name ~= vim.fs.basename(name) then
    resolved_bridge = name
    return name
  end
  local asset = asset_name()
  if asset then
    local stored = vim.fs.joinpath(storage_dir(), asset)
    local stored_stat = vim.uv.fs_stat(stored)
    if stored_stat and stored_stat.type == "file" then
      resolved_bridge = stored
      return stored
    end
  end
  local found = on_path(name)
  if found then
    resolved_bridge = found
    return found
  end
  local cargo = cargo_bin(name)
  local cargo_stat = vim.uv.fs_stat(cargo)
  if cargo_stat and cargo_stat.type == "file" then
    resolved_bridge = cargo
    return cargo
  end
  vim.notify("godot-bridge: bridge binary not found; " .. INSTALL_HINT, vim.log.levels.WARN)
  resolved_bridge = name
  return name
end

local function bridge_env()
  local settings = type(config.settings) == "table" and next(config.settings) ~= nil and config.settings or vim.empty_dict()
  return { GODOT_BRIDGE_SETTINGS = vim.json.encode(settings) }
end

-- nvim-dap hands options.env straight to uv.spawn: it replaces the environment and must be a list of NAME=VALUE.
local function dap_env()
  local env = {}
  for name, value in pairs(vim.tbl_extend("force", vim.fn.environ(), bridge_env())) do
    if not name:find("=", 1, true) then
      env[#env + 1] = name .. "=" .. value
    end
  end
  return env
end

local function is_godot_file(path)
  return type(path) == "string" and (path:match("%.gd$") ~= nil or path:match("%.tscn$") ~= nil)
end

local last_godot_file

local function remember_godot_file()
  local file = vim.api.nvim_buf_get_name(0)
  if is_godot_file(file) then
    last_godot_file = file
  end
end

local function resolve_dap_file(config)
  if is_godot_file(config and config.file) then
    return config.file
  end
  local current = vim.api.nvim_buf_get_name(0)
  if is_godot_file(current) then
    return current
  end
  return last_godot_file
end

local function project_of(dir)
  local marker = vim.fs.find("project.godot", { path = dir, upward = true })[1]
  return marker and vim.fs.dirname(marker) or nil
end

local function root_of(file)
  if not file or file == "" then
    return nil
  end
  local root = project_of(vim.fs.dirname(file))
  if not root then
    vim.notify("godot-bridge: no project.godot above " .. file, vim.log.levels.WARN)
  end
  return root
end

local function notify_result(label, result)
  local text = vim.trim((result.stdout or "") .. (result.stderr or ""))
  local level = result.code == 0 and vim.log.levels.INFO or vim.log.levels.ERROR
  vim.notify(("godot-bridge %s: exit %d\n%s"):format(label, result.code, text), level)
end

local function run_in_root(args)
  local root = root_of(vim.api.nvim_buf_get_name(0))
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
  local root = root_of(vim.api.nvim_buf_get_name(bufnr or 0))
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

function M.install()
  if installing then
    vim.notify("godot-bridge: install already running", vim.log.levels.WARN)
    return
  end
  installing = true
  local uname = vim.uv.os_uname()
  local windows = uname.sysname == "Windows_NT"
  if uname.sysname == "Darwin" then
    installing = false
    vim.notify(MACOS_MSG, vim.log.levels.ERROR)
    return
  end
  local asset = asset_name()
  local dir = storage_dir()
  local sums_tmp = vim.fn.tempname()
  local part = asset and vim.fs.joinpath(dir, asset .. "." .. vim.uv.os_getpid() .. ".part") or ""
  local final = asset and vim.fs.joinpath(dir, asset) or ""
  local function fail(msg)
    os.remove(sums_tmp)
    os.remove(part)
    installing = false
    vim.notify(msg .. ". Install it: " .. CARGO_LINE, vim.log.levels.ERROR)
  end
  if not asset then
    local arch_raw = uname.machine
    if windows then
      arch_raw = vim.env.PROCESSOR_ARCHITECTURE or arch_raw
    end
    fail(("godot-bridge: no prebuilt binary for %s/%s"):format(uname.sysname or "unknown", arch_raw or "unknown"))
    return
  end
  local curl = on_path("curl")
  local hasher = on_path(windows and "certutil" or "sha256sum")
  if not curl or not hasher then
    fail("godot-bridge: :GodotBridgeInstall needs curl and " .. (windows and "certutil" or "sha256sum") .. " on PATH")
    return
  end
  vim.fn.mkdir(dir, "p")
  local tag = "v" .. VERSION
  local sums_url = RELEASE_BASE .. "/" .. tag .. "/SHA256SUMS"
  local asset_url = RELEASE_BASE .. "/" .. tag .. "/" .. asset
  local expected

  local function hash_file(path, on_hash)
    local cmd = windows and { hasher, "-hashfile", path, "SHA256" } or { hasher, path }
    vim.system(cmd, {}, vim.schedule_wrap(function(result)
      local hash = result.code == 0 and first_hash((result.stdout or "") .. (result.stderr or "")) or nil
      on_hash(hash)
    end))
  end

  local function verify_version(path)
    vim.system({ path, "--version" }, {}, vim.schedule_wrap(function(v_result)
      local got = v_result.code == 0 and (v_result.stdout or ""):match("godot%-bridge (%d+%.%d+)")
      local want = VERSION:match("%d+%.%d+")
      if not got or got ~= want then
        os.remove(path)
        fail(("godot-bridge: plugin %s, bridge %s"):format(VERSION, got or "unknown"))
        return
      end
      resolved_bridge = nil
      installing = false
      vim.notify(("godot-bridge: installed %s"):format(asset), vim.log.levels.INFO)
      if vim.bo.filetype == "gdscript" then
        start_lsp(0)
      end
    end))
  end

  local function finish()
    if not windows then
      vim.uv.fs_chmod(final, 493)
    end
    verify_version(final)
  end

  local function place()
    for _, client in ipairs(vim.lsp.get_clients({ name = "godot" })) do
      client:stop()
    end
    vim.wait(2000, function()
      return #vim.lsp.get_clients({ name = "godot" }) == 0
    end)
    local ok = vim.uv.fs_rename(part, final)
    if not ok then
      hash_file(final, function(existing)
        if existing == expected:lower() then
          os.remove(part)
          finish()
        else
          fail(("godot-bridge: could not replace %s; stop the language server and retry"):format(final))
        end
      end)
      return
    end
    finish()
  end

  local function hash_part()
    hash_file(part, function(actual)
      if not actual then
        fail(("godot-bridge: failed to hash %s"):format(part))
        return
      end
      if actual ~= expected:lower() then
        fail(("godot-bridge: SHA-256 mismatch for %s: expected %s, got %s"):format(asset, expected:lower(), actual))
        return
      end
      place()
    end)
  end

  vim.notify(("godot-bridge: downloading SHA256SUMS for %s"):format(tag), vim.log.levels.INFO)
  vim.system({ curl, "-fsSL", "--proto", "=https", "--proto-redir", "=https", "--retry", "3", "-o", sums_tmp, sums_url }, {}, vim.schedule_wrap(function(sums_result)
    if sums_result.code ~= 0 then
      fail(("godot-bridge: failed to download SHA256SUMS for %s"):format(tag))
      return
    end
    local handle = io.open(sums_tmp, "r")
    local contents = handle and handle:read("*a")
    if handle then
      handle:close()
    end
    os.remove(sums_tmp)
    if not contents then
      fail(("godot-bridge: failed to read SHA256SUMS for %s"):format(tag))
      return
    end
    local found, matches = sums_hash(contents, asset)
    expected = found
    if matches == 0 then
      fail(("godot-bridge: %s absent from release %s"):format(asset, tag))
      return
    elseif matches > 1 then
      fail(("godot-bridge: SHA256SUMS for %s is corrupt"):format(tag))
      return
    end
    vim.notify(("godot-bridge: downloading %s"):format(asset), vim.log.levels.INFO)
    vim.system({ curl, "-fsSL", "--proto", "=https", "--proto-redir", "=https", "--retry", "3", "-o", part, asset_url }, {}, vim.schedule_wrap(function(dl_result)
      if dl_result.code ~= 0 then
        fail(("godot-bridge: failed to download %s"):format(asset))
        return
      end
      hash_part()
    end))
  end))
end

function M.setup(opts)
  config = vim.tbl_extend("force", config, type(opts) == "table" and opts or {})
  resolved_bridge = nil
  if vim.uv.os_uname().sysname == "Darwin" then
    vim.notify(MACOS_MSG, vim.log.levels.ERROR)
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
  if pcall(require, "dap") then
    M.dap()
  end
end

local function current_scene_or_script()
  local file = vim.api.nvim_buf_get_name(0)
  if not is_godot_file(file) then
    vim.notify("godot-bridge: open a .gd or .tscn file first.", vim.log.levels.WARN)
    return nil
  end
  return file
end

function M.run()
  local file = current_scene_or_script()
  if not file then
    return
  end
  run_in_root({ "run", "--file", file })
end

function M.run_scene()
  local file = current_scene_or_script()
  if not file then
    return
  end
  run_in_root({ "run", "--file", file, "--scene", "current" })
end

function M.open_editor()
  local file = current_scene_or_script()
  if not file then
    return
  end
  run_in_root({ "open-editor", "--file", file })
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

local DEFAULT_DAP_CONFIGURATIONS = {
  { type = "godot", request = "launch", name = "Godot: run project", scene = "main" },
  { type = "godot", request = "launch", name = "Godot: run current scene", scene = "current" },
  { type = "godot", request = "attach", name = "Godot: attach" },
}

local function initialize_timeout_s()
  local settings = type(config.settings) == "table" and config.settings or {}
  local startup = tonumber(settings.startup_timeout_s) or 600
  return startup == 0 and 3600 or startup + 40
end

local function merge_dap_configurations(existing)
  local merged = {}
  local seen = {}
  if type(existing) == "table" then
    for _, item in ipairs(existing) do
      merged[#merged + 1] = item
      if type(item) == "table" and type(item.name) == "string" then
        seen[item.name] = true
      end
    end
  end
  for _, item in ipairs(DEFAULT_DAP_CONFIGURATIONS) do
    if not seen[item.name] then
      merged[#merged + 1] = vim.deepcopy(item)
      seen[item.name] = true
    end
  end
  return merged
end

function M.dap()
  local ok, dap = pcall(require, "dap")
  if not ok then
    vim.notify("godot-bridge: nvim-dap is not installed", vim.log.levels.ERROR)
    return nil
  end
  vim.api.nvim_create_autocmd("BufEnter", {
    group = vim.api.nvim_create_augroup("godot-bridge-dap", { clear = true }),
    callback = remember_godot_file,
  })
  dap.adapters.godot = function(callback, config)
    if dap.session() or next(dap.sessions()) then
      vim.notify("godot-bridge: a debug session is already running", vim.log.levels.WARN)
      return
    end
    local file = resolve_dap_file(config)
    local cwd = root_of(file) or project_of(vim.fn.getcwd()) or vim.fn.getcwd()
    callback({
      type = "executable",
      command = bridge(),
      args = file and { "dap", "--file", file } or { "dap" },
      options = {
        cwd = cwd,
        env = dap_env(),
        initialize_timeout_sec = initialize_timeout_s(),
      },
    })
  end
  dap.configurations.gdscript = merge_dap_configurations(dap.configurations.gdscript)
  return dap
end

function M.debug()
  local dap = M.dap()
  if dap then
    dap.continue()
  end
end

return M

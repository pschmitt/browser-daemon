{
  lib,
  buildPythonPackage,
  setuptools,
  aiohttp,
  pytestCheckHook,
  pytest-asyncio,
}:
buildPythonPackage {
  pname = "browser-daemon-client";
  # The daemon's version: the client is released with it.
  version = (builtins.fromTOML (builtins.readFile ../python/pyproject.toml)).project.version;
  pyproject = true;
  # The tests of the Home Assistant integration read ../custom_components.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../python
      ../custom_components
    ];
  };
  sourceRoot = "source/python";

  build-system = [ setuptools ];
  dependencies = [ aiohttp ];

  nativeCheckInputs = [
    pytestCheckHook
    pytest-asyncio
  ];
  pythonImportsCheck = [ "browser_daemon_client" ];

  meta = {
    description = "Async client for browser-daemon: Playwright runs over HTTP, sessions and logins";
    homepage = "https://github.com/pschmitt/browser-daemon";
    license = lib.licenses.gpl3Only;
  };
}

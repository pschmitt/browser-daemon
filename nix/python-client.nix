{ python3Packages }:
python3Packages.buildPythonPackage {
  pname = "browser-daemon-client";
  version = (builtins.fromTOML (builtins.readFile ../python/pyproject.toml)).project.version;
  pyproject = true;
  src = ../python;

  build-system = [ python3Packages.setuptools ];
  dependencies = [ python3Packages.aiohttp ];

  nativeCheckInputs = with python3Packages; [
    pytestCheckHook
    pytest-asyncio
  ];
  pythonImportsCheck = [ "browser_daemon_client" ];

  meta = {
    description = "Async client for browser-daemon: Playwright runs over HTTP, sessions and logins";
    homepage = "https://github.com/pschmitt/browser-daemon";
    license = python3Packages.lib.licenses.gpl3Only;
  };
}

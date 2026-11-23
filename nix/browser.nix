{
  pkgs,
  # For the local tab API's native messaging host (docs/local-api.md).
  browser-daemon,
}:
let
  inherit ((fromTOML (builtins.readFile ../Cargo.toml)).package) version;
  addonId = "browser-daemon-node@pschmitt.dev";
  metadata = builtins.fromJSON (builtins.readFile ../browser/packages/metadata.json);
  # The signed packages are release assets, pinned by hash in metadata.json
  # (the release workflow updates it after signing).
  releaseAsset =
    file: sha256:
    pkgs.fetchurl {
      url = "https://github.com/pschmitt/browser-daemon/releases/download/${metadata.tag}/${file}";
      inherit sha256;
    };
  firefoxXpi = releaseAsset "browser-daemon-node-firefox-${metadata.version}.xpi" metadata.firefoxSha256;
  chromiumCrx = releaseAsset "browser-daemon-node-chromium-${metadata.version}.crx" metadata.chromiumSha256;
  hostName = "dev.pschmitt.browser_daemon";
  companion = pkgs.runCommand "browser-daemon-firefox-companion-${version}" { } ''
    install -Dm755 ${../browser/companion.py} $out/bin/browser-daemon-firefox-companion
    substituteInPlace $out/bin/browser-daemon-firefox-companion \
      --replace-fail '#!/usr/bin/env python3' '#!${pkgs.python3}/bin/python3'
  '';
  manifest = (pkgs.formats.json { }).generate "${hostName}.json" {
    name = hostName;
    description = "Selected-tab Firefox control for Browser Daemon Node";
    path = "${companion}/bin/browser-daemon-firefox-companion";
    type = "stdio";
    allowed_extensions = [ addonId ];
  };
  # The local tab API: Firefox and Chromium-based browsers.
  localHostName = "dev.pschmitt.browser_daemon_local";
  localManifest =
    allowed:
    (pkgs.formats.json { }).generate "${localHostName}.json" (
      {
        name = localHostName;
        description = "Browser Daemon local tab API";
        path = "${browser-daemon}/bin/browser-daemon-native-host";
        type = "stdio";
      }
      // allowed
    );
  localFirefox = localManifest { allowed_extensions = [ addonId ]; };
  localChromium = localManifest {
    allowed_origins = [ "chrome-extension://${metadata.chromiumId}/" ];
  };
in
{
  # Firefox: the companion and the local tab API. Chromium-based browsers: the
  # local tab API (the companion is Firefox-only).
  browser-native-host = pkgs.runCommand "browser-daemon-native-host-${version}" { } ''
    mkdir -p $out/lib/mozilla/native-messaging-hosts \
      $out/etc/chromium/native-messaging-hosts $out/lib/chromium/NativeMessagingHosts
    ln -s ${manifest} $out/lib/mozilla/native-messaging-hosts/${hostName}.json
    ln -s ${localFirefox} $out/lib/mozilla/native-messaging-hosts/${localHostName}.json
    ln -s ${localChromium} $out/etc/chromium/native-messaging-hosts/${localHostName}.json
    ln -s ${localChromium} $out/lib/chromium/NativeMessagingHosts/${localHostName}.json
  '';

  firefox-addon =
    pkgs.runCommand "browser-daemon-firefox-addon-${metadata.version}"
      {
        inherit (metadata) version;
        passthru = { inherit addonId; };
      }
      ''
        mkdir -p $out/share/mozilla/extensions/{ec8030f7-c20a-464f-9b0e-13a3a9e97384}
        cp ${firefoxXpi} $out/${addonId}.xpi
        ln -s $out/${addonId}.xpi \
          $out/share/mozilla/extensions/{ec8030f7-c20a-464f-9b0e-13a3a9e97384}/${addonId}.xpi
      '';

  chromium-addon =
    pkgs.runCommand "browser-daemon-chromium-addon-${metadata.version}"
      {
        inherit (metadata) version;
        passthru.extensionId = metadata.chromiumId;
      }
      ''
        mkdir -p $out
        cp ${chromiumCrx} $out/browser-daemon-node.crx
      '';
}

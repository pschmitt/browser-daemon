# Ad blockers for browser-daemon sessions: uBlock Origin Lite (Manifest V3,
# since current Chromium no longer runs MV2 extensions) and the full uBlock
# Origin for Firefox.
{
  lib,
  stdenvNoCC,
  fetchurl,
  unzip,
}:
let
  # renovate: datasource=github-releases depName=uBlockOrigin/uBOL-home
  ubolVersion = "2026.1006.1931";
  # renovate: datasource=github-releases depName=gorhill/uBlock
  uboVersion = "1.75.0";
in
{
  ublock-origin-lite = stdenvNoCC.mkDerivation {
    pname = "ublock-origin-lite-chromium";
    version = ubolVersion;
    src = fetchurl {
      url = "https://github.com/uBlockOrigin/uBOL-home/releases/download/${ubolVersion}/uBOLite_${ubolVersion}.chromium.zip";
      hash = "sha256-FnD5JZD1rVsg8C0KdeFEVyVntLqXmz3DIExB9lEgb+c=";
    };
    nativeBuildInputs = [ unzip ];
    unpackPhase = ''
      runHook preUnpack
      mkdir -p $out
      unzip -q $src -d $out
      runHook postUnpack
    '';
    dontBuild = true;
    dontInstall = true;
    dontFixup = true;
    meta = {
      description = "uBlock Origin Lite, unpacked for Chromium's --load-extension";
      homepage = "https://github.com/uBlockOrigin/uBOL-home";
      license = lib.licenses.gpl3Plus;
    };
  };

  ublock-origin-xpi = stdenvNoCC.mkDerivation {
    pname = "ublock-origin-firefox";
    version = uboVersion;
    src = fetchurl {
      url = "https://github.com/gorhill/uBlock/releases/download/${uboVersion}/uBlock0_${uboVersion}.firefox.signed.xpi";
      hash = "sha256-W3RBWGBFY3BkS9gPFhJehlsObDVrtd/PuEBpln6qUoc=";
    };
    dontUnpack = true;
    installPhase = ''
      runHook preInstall
      install -Dm644 $src $out/ublock-origin.xpi
      runHook postInstall
    '';
    meta = {
      description = "uBlock Origin add-on for Firefox";
      homepage = "https://github.com/gorhill/uBlock";
      license = lib.licenses.gpl3Plus;
    };
  };
}

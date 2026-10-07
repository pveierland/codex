# Prebuilt V8 for the `v8` crate, which would otherwise download it during the build.
#
# The workspace enables V8's sandbox, which upstream rusty_v8 does not publish prebuilt, so
# this fetches the Codex-built release assets that Cargo release builds use (see
# .github/actions/setup-rusty-v8). Their checksums are the verified ones checked in under
# third_party/v8, so a `v8` version bump needs no change here.
{
  fetchurl,
  lib,
  stdenv,
}:
let
  version =
    (lib.findFirst (package: package.name == "v8") (throw "Cargo.lock has no v8 package") (
      builtins.fromTOML (builtins.readFile ../Cargo.lock)
    ).package).version;
  checksumFile = ../../third_party/v8 + "/rusty_v8_${lib.replaceStrings [ "." ] [ "_" ] version}.sha256";

  # "<sha256>  <file name>" lines, as `sha256sum` writes them.
  checksums = lib.listToAttrs (
    map (
      line:
      let
        fields = lib.splitString "  " line;
      in
      lib.nameValuePair (lib.elemAt fields 1) (lib.elemAt fields 0)
    ) (lib.filter (line: line != "") (lib.splitString "\n" (builtins.readFile checksumFile)))
  );

  target = stdenv.hostPlatform.rust.rustcTarget;
  profile = "ptrcomp_sandbox_release";
  release = "https://github.com/openai/codex/releases/download/rusty-v8-v${version}";
  fetchAsset =
    name:
    fetchurl {
      url = "${release}/${name}";
      sha256 = checksums.${name} or (throw "no Codex-built V8 ${version} asset ${name}");
    };
in
{
  # Nix systems whose targets have Codex-built Unix archives.
  platforms = lib.filter (
    system:
    checksums ? "librusty_v8_${profile}_${(lib.systems.elaborate system).rust.rustcTarget}.a.gz"
  ) [
    "x86_64-linux"
    "aarch64-linux"
    "x86_64-darwin"
    "aarch64-darwin"
  ];
  archive = fetchAsset "librusty_v8_${profile}_${target}.a.gz";
  srcBinding = fetchAsset "src_binding_${profile}_${target}.rs";
}

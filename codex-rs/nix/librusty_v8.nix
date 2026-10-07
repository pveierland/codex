# Prebuilt V8 for the `v8` crate, which would otherwise download it during the build.
#
# When Cargo.lock moves to another `v8` version, update `version` and the hashes:
#   nix-prefetch-url https://github.com/denoland/rusty_v8/releases/download/v<version>/librusty_v8_release_<target>.a.gz
#   nix-prefetch-url https://github.com/denoland/rusty_v8/releases/download/v<version>/src_binding_release_<target>.rs
{
  fetchurl,
  lib,
  stdenv,
}:
let
  version = "150.4.0";
  archives = {
    x86_64-linux = "0v5hi3s56b6yk7nh5n0wygh7fn0j41yyjz5903r227qv0yvzssaq";
    aarch64-linux = "1lvx9xjzv7ibqvg5jnaxqaaim0lw4dwfgf6kw0pjfdrkmm97s5xp";
    aarch64-darwin = "043bgs3hcvrn1yzknrxchqnki8r9p7ggk9zbiaqwa8mqhlagin6c";
  };
  srcBindings = {
    x86_64-linux = "01l53l6nk4p5brpz2v3svqijx3hz5nqry8q7x12vdgbrwim849vp";
    aarch64-linux = "01l53l6nk4p5brpz2v3svqijx3hz5nqry8q7x12vdgbrwim849vp";
    aarch64-darwin = "0krrb2vh4skvfmzwpcqkl55bg2gyn943drqa8snp16lwz06dynna";
  };

  lockedVersion =
    (lib.findFirst (package: package.name == "v8") (throw "Cargo.lock has no v8 package") (
      builtins.fromTOML (builtins.readFile ../Cargo.lock)
    ).package).version;
  system = stdenv.hostPlatform.system;
  target = stdenv.hostPlatform.rust.rustcTarget;
  release = "https://github.com/denoland/rusty_v8/releases/download/v${version}";
in
assert lib.assertMsg (lockedVersion == version)
  "codex-rs/nix/librusty_v8.nix is for v8 ${version} but Cargo.lock uses ${lockedVersion}; update it.";
{
  inherit archives;
  archive = fetchurl {
    name = "librusty_v8-${version}";
    url = "${release}/librusty_v8_release_${target}.a.gz";
    sha256 = archives.${system} or (throw "no prebuilt V8 ${version} for ${system}");
  };
  srcBinding = fetchurl {
    name = "src_binding-${version}";
    url = "${release}/src_binding_release_${target}.rs";
    sha256 = srcBindings.${system} or (throw "no prebuilt V8 bindings ${version} for ${system}");
  };
}

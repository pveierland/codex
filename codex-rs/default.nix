{
  callPackage,
  cmake,
  libcap ? null,
  libclang,
  llvmPackages,
  openssl,
  pkg-config,
  ripgrep,
  rustPlatform,
  lib,
  stdenv,
  versionCheckHook,
  writableTmpDirAsHomeHook,
  version ? "0.0.0",
  ...
}:
let
  rustyV8 = callPackage ./nix/librusty_v8.nix { };
  target = stdenv.hostPlatform.rust.rustcTarget;
in
rustPlatform.buildRustPackage (_: {
  pname = "codex-rs";
  inherit version;
  src = ./.;

  # Git dependencies are pinned by revision in Cargo.lock, so they need no separate hashes.
  cargoLock = {
    lockFile = ./Cargo.lock;
    allowBuiltinFetchGit = true;
  };

  # The released CLI: the `codex` entrypoint, its out-of-process V8 host and, on Linux, the
  # bubblewrap sandbox it bundles.
  cargoBuildFlags = [
    "--package"
    "codex-cli"
    "--package"
    "codex-code-mode-host"
  ]
  ++ lib.optionals stdenv.isLinux [
    "--package"
    "codex-bwrap"
  ];
  doCheck = false;

  # Patch the workspace Cargo.toml so that cargo embeds the correct version in
  # CARGO_PKG_VERSION (which the binary reads via env!("CARGO_PKG_VERSION")).
  # On release commits the Cargo.toml already contains the real version and
  # this sed is a no-op.
  postPatch = ''
    sed -i 's/^version = "0\.0\.0"$/version = "${version}"/' Cargo.toml
  '';

  nativeBuildInputs = [
    cmake
    llvmPackages.clang
    pkg-config
  ];
  buildInputs = [ openssl ] ++ lib.optionals stdenv.isLinux [ libcap ];

  env = {
    LIBCLANG_PATH = "${lib.getLib libclang}/lib";
    # The v8 crate otherwise downloads these during the build.
    RUSTY_V8_ARCHIVE = rustyV8.archive;
    RUSTY_V8_SRC_BINDING_PATH = rustyV8.srcBinding;
  };

  # Install the standalone package layout (see scripts/codex_package/layout.py) under
  # libexec/codex: the app-server daemon copies this package, so helpers are real files
  # inside it rather than links into other store paths.
  postInstall = ''
    package=$out/libexec/codex
    mkdir -p $package/bin $package/codex-path $package/codex-resources
    mv $out/bin/codex $out/bin/codex-code-mode-host $package/bin/
    rm -f $out/bin/logs_client
    install -m755 ${lib.getExe ripgrep} $package/codex-path/rg
    ${lib.optionalString stdenv.isLinux "mv $out/bin/bwrap $package/codex-resources/"}
    cat > $package/codex-package.json <<EOF
    {
      "layoutVersion": 1,
      "version": "${version}",
      "target": "${target}",
      "variant": "codex",
      "entrypoint": "bin/codex",
      "resourcesDir": "codex-resources",
      "pathDir": "codex-path"
    }
    EOF
    ln -s $package/bin/codex $out/bin/codex
  '';

  doInstallCheck = true;
  nativeInstallCheckInputs = [
    versionCheckHook
    writableTmpDirAsHomeHook
  ];
  versionCheckProgramArg = "--version";

  meta = with lib; {
    description = "OpenAI Codex command‑line interface rust implementation";
    license = licenses.asl20;
    homepage = "https://github.com/openai/codex";
    mainProgram = "codex";
    platforms = builtins.attrNames rustyV8.archives;
  };
})

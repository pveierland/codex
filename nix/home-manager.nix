# Extends Home Manager's `programs.codex` so the CLI and the app-server daemon
# it auto-starts both run this flake's package.
#
# The daemon normally runs a release it copied into
# CODEX_HOME/packages/app-server-daemon and keeps replacing with downloaded
# releases, so a CLI installed by Nix never reaches the daemon. This module
# selects the Nix package there instead, keeps the updater off, and restarts
# a running daemon whose executable is not the selected package.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.codex;
  daemonCfg = cfg.appServerDaemon;

  # Home Manager's codex module sets CODEX_HOME when it uses XDG directories.
  codexHome = config.home.sessionVariables.CODEX_HOME or "${config.home.homeDirectory}/.codex";

  # Mirrors when Home Manager's codex module writes a settings file.
  writesSettings =
    (cfg.settings != null && cfg.settings != { })
    || (
      cfg.enableMcpIntegration
      && (config.programs.mcp.enable or false)
      && config.programs.mcp.servers != { }
    );

  selectPackage = pkgs.writeShellApplication {
    name = "codex-app-server-daemon-select";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.jq
      pkgs.util-linux
    ];
    text = ''
      package=${cfg.package}/libexec/codex
      root="${codexHome}/packages/app-server-daemon"
      settings="${codexHome}/app-server-daemon/settings.json"

      if [[ ! -f $package/codex-package.json ]]; then
        echo "$package is not a Codex release package; programs.codex.appServerDaemon requires the codex flake's package" >&2
        exit 1
      fi
      mkdir -p "$root" "''${settings%/*}"

      # Hold the lock the daemon and its installers take while selecting a package.
      exec 9>>"$root/install.lock"
      if ! flock --timeout 30 9; then
        echo "timed out after 30 seconds waiting for $root/install.lock" >&2
        exit 1
      fi

      # Disable the updater before selecting, so it cannot select a downloaded release again.
      if [[ ! -e $settings || $(jq '.updater.autoUpdateEnabled' "$settings") != false ]]; then
        updated=$(mktemp "$settings.XXXXXX")
        trap 'rm -f "$updated"' EXIT
        if [[ -e $settings ]]; then
          jq '.updater.autoUpdateEnabled = false' "$settings" >"$updated"
        else
          jq --null-input '{updater: {autoUpdateEnabled: false}}' >"$updated"
        fi
        mv "$updated" "$settings"
      fi
      rm -f "$root/auto-update-version"

      if [[ $(readlink "$root/current" || true) != "$package" ]]; then
        ln -sfn "$package" "$root/current.new"
        mv --no-target-directory "$root/current.new" "$root/current"
      fi
    '';
  };

  # Tools are referenced by path because the restarted daemon inherits PATH.
  restartChanged =
    let
      codex = "${cfg.package}/bin/codex";
      jq = lib.getExe pkgs.jq;
      readlink = "${pkgs.coreutils}/bin/readlink";
    in
    pkgs.writeShellApplication {
      name = "codex-app-server-daemon-restart-changed";
      text = ''
        export CODEX_HOME="${codexHome}"

        # Leave a stopped daemon to the next client, and never touch an app server
        # that `codex app-server daemon` does not manage.
        if ! status=$(${codex} app-server daemon version 2>/dev/null) \
          || [[ $(${jq} --raw-output '.backend // empty' <<<"$status") != pid ]]; then
          exit 0
        fi

        # Compare executables rather than versions: rebuilds of one revision share a
        # version, and a daemon must not outlive the store path it runs from.
        pid=$(${jq} --raw-output '.pid' "$CODEX_HOME/app-server-daemon/daemon.pid")
        running=$(${readlink} "/proc/$pid/exe" || true)
        selected=$(${readlink} --canonicalize "$CODEX_HOME/packages/app-server-daemon/current/bin/codex")
        if [[ $running == "$selected" ]]; then
          exit 0
        fi

        echo "Restarting the Codex app-server daemon from $selected (was $running)"
        ${codex} app-server daemon restart
      '';
    };
in
{
  _class = "homeManager";

  options.programs.codex.appServerDaemon = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = pkgs.stdenv.hostPlatform.isLinux;
      defaultText = lib.literalExpression "pkgs.stdenv.hostPlatform.isLinux";
      example = false;
      description = ''
        Whether the local app-server daemon that Codex auto-starts runs
        {option}`programs.codex.package` instead of releases downloaded by its
        auto-updater. The package must be this flake's Codex package. Only
        Linux is supported.
      '';
    };

    restartOnChange = lib.mkOption {
      type = lib.types.bool;
      default = true;
      example = false;
      description = ''
        Whether activation restarts a running daemon that does not run
        {option}`programs.codex.package`. The restart lets running turns drain
        for the daemon's shutdown grace period and resumes their threads in the
        new daemon. When disabled, run `codex app-server daemon restart` to
        adopt a new package.
      '';
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        programs.codex.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;

        assertions = [
          {
            # Home Manager picks the settings format and XDG location by package
            # version, and this flake's development builds are versioned 0.0.0-dev.
            assertion =
              !writesSettings || cfg.package == null || lib.versionAtLeast (lib.getVersion cfg.package) "0.2.0";
            message = "`programs.codex.settings` and `programs.codex.enableMcpIntegration` need a release-versioned `programs.codex.package`; development builds of the codex flake would write config.yaml instead of config.toml";
          }
        ];
      }

      (lib.mkIf daemonCfg.enable {
        assertions = [
          {
            assertion = cfg.package != null;
            message = "`programs.codex.appServerDaemon` requires `programs.codex.package`";
          }
          {
            assertion = pkgs.stdenv.hostPlatform.isLinux;
            message = "`programs.codex.appServerDaemon` is only supported on Linux";
          }
        ];

        # A failed selection leaves the daemon on its current package, which the
        # restart unit then keeps, so it must not abort the rest of activation.
        home.activation.codexAppServerDaemon =
          lib.hm.dag.entryBetween [ "reloadSystemd" ] [ "linkGeneration" ]
            ''
              run ${lib.getExe selectPackage} \
                || warnEcho "Could not select the Codex app-server daemon package; the daemon keeps its current package"
            '';

        # Activation restarts this unit when the package changes, after selecting it above.
        systemd.user.services.codex-app-server-daemon-restart = lib.mkIf daemonCfg.restartOnChange {
          Unit.Description = "Restart the Codex app-server daemon on the selected package";
          Install.WantedBy = [ "default.target" ];
          Service = {
            Type = "oneshot";
            RemainAfterExit = true;
            # The restarted daemon joins this unit's cgroup. It must outlive the
            # restart command and later restarts of this unit.
            KillMode = "process";
            # The daemon runs agent commands, so start it with a login environment
            # like the terminals that otherwise start it.
            ExecStart = "${lib.getExe pkgs.bashInteractive} --login -c 'exec ${lib.getExe restartChanged}'";
          };
        };
      })
    ]
  );
}

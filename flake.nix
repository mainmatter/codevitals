{
  description = "tally: facts about a git repository's history, in DuckDB";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;
      defaultSystems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forAllSystems =
        fn:
        lib.genAttrs defaultSystems (
          system:
          let
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            };
          in
          fn pkgs
        );
    in
    {
      # `nix run github:mainmatter/tally -- scan`: the CLI, built with the pinned toolchain.
      packages = forAllSystems (
        pkgs:
        let
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
        in
        {
          default = rustPlatform.buildRustPackage {
            pname = "tally";
            version = (lib.importTOML ./Cargo.toml).package.version;
            src = lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            # The tests need git and take minutes in a sandbox; CI runs them with cargo.
            doCheck = false;
            meta.mainProgram = "tally";
          };
        }
      );

      devShells = forAllSystems (
        pkgs:
        let
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          # Grafana's plugin directory: one subdirectory per plugin id.
          grafanaPlugins = pkgs.linkFarm "tally-grafana-plugins" [
            {
              name = "yesoreyeram-infinity-datasource";
              path = pkgs.grafanaPlugins.yesoreyeram-infinity-datasource;
            }
          ];

          # A throwaway local Grafana showing one `tally metrics` JSON file with the
          # prebuilt dashboard. State lives in a temporary directory; Ctrl-C stops it.
          tally-grafana = pkgs.writeShellApplication {
            name = "tally-grafana";
            runtimeInputs = [
              pkgs.grafana
              pkgs.python3
            ];
            text = ''
              metrics="''${1:-metrics.json}"
              if [ ! -f "$metrics" ]; then
                echo "usage: tally-grafana [metrics.json]  (write it with: tally metrics metrics.json)" >&2
                exit 1
              fi

              state="$(mktemp -d)"
              mkdir -p "$state/www"
              ln -s "$(realpath "$metrics")" "$state/www/metrics.json"

              # Infinity fetches from the Grafana server, so serving on loopback is enough.
              python3 -m http.server 8123 --bind 127.0.0.1 --directory "$state/www" >/dev/null 2>&1 &
              server=$!
              trap 'kill "$server"; rm -rf "$state"' EXIT

              echo "tally dashboard: http://127.0.0.1:3000/d/tally (serving $metrics)" >&2
              GF_PATHS_DATA="$state/data" \
              GF_PATHS_LOGS="$state/logs" \
              GF_PATHS_PLUGINS="${grafanaPlugins}" \
              GF_PATHS_PROVISIONING="${./grafana/provisioning}" \
              TALLY_DASHBOARDS="${./grafana/dashboards}" \
              GF_DASHBOARDS_DEFAULT_HOME_DASHBOARD_PATH="${./grafana/dashboards}/tally.json" \
              GF_SERVER_HTTP_ADDR=127.0.0.1 \
              GF_SERVER_HTTP_PORT=3000 \
              GF_AUTH_ANONYMOUS_ENABLED=true \
              GF_AUTH_ANONYMOUS_ORG_ROLE=Admin \
              GF_AUTH_DISABLE_LOGIN_FORM=true \
              GF_ANALYTICS_REPORTING_ENABLED=false \
              GF_ANALYTICS_CHECK_FOR_UPDATES=false \
              GF_NEWS_NEWS_FEED_ENABLED=false \
              GF_LOG_LEVEL=warn \
                grafana server --homepath "${pkgs.grafana}/share/grafana"
            '';
          };

          # Tools every CI job needs; keep this small for cold runners.
          ciInputs = [
            rustToolchain
            pkgs.git
          ];

          # Tools only useful in an interactive shell.
          devOnlyInputs = [
            pkgs.duckdb
            pkgs.python3 # grafana/generate.py
            tally-grafana
          ];
        in
        {
          default = pkgs.mkShell {
            name = "tally-dev";
            buildInputs = ciInputs ++ devOnlyInputs;
          };

          ci = pkgs.mkShell {
            name = "tally-ci";
            buildInputs = ciInputs;
          };
        }
      );
    };
}

{
  description = "Njalla DNS webhook provider for external-dns";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
          targets = [ "x86_64-unknown-linux-musl" ];
        };

        # Build the njalla-webhook binary
        njalla-webhook = pkgs.rustPlatform.buildRustPackage {
          pname = "njalla-webhook";
          version = "0.1.0";
          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
            rustToolchain
          ];

          buildInputs = with pkgs; [
            openssl
            openssl.dev
          ] ++ lib.optionals stdenv.isDarwin [
            darwin.apple_sdk.frameworks.Security
            darwin.apple_sdk.frameworks.SystemConfiguration
          ];

          # Build for musl target for static linking
          CARGO_BUILD_TARGET = "x86_64-unknown-linux-musl";
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "${pkgs.pkgsStatic.stdenv.cc}/bin/${pkgs.pkgsStatic.stdenv.cc.targetPrefix}cc";

          # Set OpenSSL environment variables for static linking
          OPENSSL_STATIC = "1";
          OPENSSL_LIB_DIR = "${pkgs.pkgsStatic.openssl}/lib";
          OPENSSL_INCLUDE_DIR = "${pkgs.pkgsStatic.openssl.dev}/include";

          # Skip tests during build (can run separately)
          doCheck = false;

          meta = with pkgs.lib; {
            description = "Njalla DNS webhook provider for external-dns";
            homepage = "https://github.com/yourusername/njalla-webhook";
            license = licenses.mit;
          };
        };
      in
      {
        # Package outputs
        packages = {
          default = njalla-webhook;

          # Docker image
          dockerImage = pkgs.dockerTools.buildImage {
            name = "njalla-webhook";
            tag = "latest";

            copyToRoot = pkgs.buildEnv {
              name = "image-root";
              paths = [
                njalla-webhook
                pkgs.bashInteractive
                pkgs.coreutils
                pkgs.curl
                pkgs.wget
                pkgs.cacert
                pkgs.jq
                pkgs.netcat
                pkgs.procps
                pkgs.htop
                pkgs.vim
                pkgs.less
                pkgs.gnugrep
                pkgs.gawk
                pkgs.gnused
                pkgs.findutils
                pkgs.which
                pkgs.net-tools
                pkgs.iputils
                pkgs.dnsutils
                pkgs.gnutar
                pkgs.file
                pkgs.busybox  # Fallback for any missing tools
              ];
              pathsToLink = [ "/bin" "/etc" "/share" ];
            };

            config = {
              Cmd = [ "/bin/njalla-webhook" ];
              ExposedPorts = {
                "8888/tcp" = {};
              };
              Env = [
                "RUST_LOG=info"
                "WEBHOOK_HOST=0.0.0.0"
                "WEBHOOK_PORT=8888"
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
                "SYSTEM_CERTIFICATE_PATH=${pkgs.cacert}/etc/ssl/certs"
                "PATH=/bin:/usr/bin:/usr/local/bin"
              ];
              Labels = {
                "org.opencontainers.image.source" = "https://github.com/yourusername/njalla-webhook";
                "org.opencontainers.image.description" = "Njalla DNS webhook provider for external-dns";
                "org.opencontainers.image.licenses" = "MIT";
              };
              Healthcheck = {
                Test = ["CMD" "${pkgs.coreutils}/bin/timeout" "5" "${pkgs.coreutils}/bin/sh" "-c" "echo -e 'GET /health HTTP/1.1\\r\\nHost: localhost\\r\\n\\r\\n' | ${pkgs.coreutils}/bin/nc 127.0.0.1 8888 | ${pkgs.coreutils}/bin/grep -q '200 OK'"];
                Interval = 30000000000; # 30 seconds in nanoseconds
                Timeout = 5000000000;   # 5 seconds in nanoseconds
                Retries = 3;
              };
            };
          };
        };

        # Development shell
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            rust-analyzer
            cargo-watch
            cargo-edit
            cargo-audit
            cargo-outdated
            pkg-config
            openssl
            openssl.dev

            # Development tools
            just
            bacon
            tokio-console

            # For API testing
            curl
            jq
            httpie
          ];

          RUST_BACKTRACE = "full";
          RUST_LOG = "debug";

          # Set OpenSSL environment variables
          OPENSSL_DIR = "${pkgs.openssl.dev}";
          OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
          OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include";
          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";

          shellHook = ''
            echo "🦀 Njalla Webhook Development Environment"
            echo ""
            echo "Available commands:"
            echo "  cargo build           - Build the project"
            echo "  cargo run             - Run the webhook server"
            echo "  cargo test            - Run tests"
            echo "  cargo watch -x run    - Auto-rebuild on changes"
            echo "  bacon                 - Background rust compiler"
            echo ""
            echo "Build Docker image:"
            echo "  nix build .#dockerImage"
            echo "  docker load < result"
            echo ""

            # Create .env from example if it doesn't exist
            if [ ! -f .env ] && [ -f .env.example ]; then
              cp .env.example .env
              echo "Created .env from .env.example - please configure your Njalla API token"
            fi
          '';
        };

        # Apps for nix run
        apps.default = flake-utils.lib.mkApp {
          drv = njalla-webhook;
        };

        # Checks for CI
        checks = {
          inherit njalla-webhook;

          # Format check
          format = pkgs.runCommand "format-check" {} ''
            cd ${./.}
            ${rustToolchain}/bin/cargo fmt --check
            touch $out
          '';

          # Clippy check
          clippy = pkgs.runCommand "clippy-check" {} ''
            cd ${./.}
            ${rustToolchain}/bin/cargo clippy --all-targets --all-features -- -D warnings
            touch $out
          '';
        };
      }
    );
}
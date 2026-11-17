{
  description = "ipmi-fan-control";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [self.overlays.default];
        };
        lib = nixpkgs.lib;

        # Evaluate a minimal host that enables the service, plus one extra module
        evalHost = extraModule:
          (lib.nixosSystem {
            modules = [
              self.nixosModules.default
              {
                nixpkgs.hostPlatform = system;
                nixpkgs.overlays = [self.overlays.default];
                fileSystems."/".device = "/dev/null";
                boot.loader.grub.enable = false;
                services.ipmi-fan-control.enable = true;
              }
              extraModule
            ];
          }).config;

        withExtra = evalHost ({pkgs, ...}: {
          services.ipmi-fan-control.extraPackages = [pkgs.hello];
        });
        withoutExtra = evalHost {};

        hasPackage = name: packages: lib.any (p: lib.getName p == name) packages;
        unit = host: host.systemd.services.ipmi-fan-control;
        hasPathEntry = host:
          lib.any (lib.hasPrefix "PATH=")
          (lib.toList ((unit host).serviceConfig.Environment or []));

        moduleFailures =
          lib.optional (!hasPackage "hello" (unit withExtra).path)
          "extraPackages: hello is not in the unit path"
          ++ lib.optional (hasPathEntry withExtra)
          "serviceConfig.Environment sets PATH"
          ++ lib.optional (!hasPackage "freeipmi" (unit withoutExtra).path)
          "default: freeipmi is not in the unit path"
          ++ lib.optional (withoutExtra.services.ipmi-fan-control.extraPackages != [])
          "default: extraPackages is not empty";
      in {
        formatter = pkgs.alejandra;

        checks = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          module-eval = pkgs.runCommand "module-eval" {} ''
            ${lib.concatMapStrings (m: "echo ${lib.escapeShellArg m} >&2\n") moduleFailures}
            ${lib.optionalString (moduleFailures != []) "exit 1"}
            touch $out
          '';
        };

        packages = {
          default = pkgs.ipmi-fan-control;
          ipmi-fan-control = pkgs.ipmi-fan-control;
        };

        devShells.default = pkgs.mkShell rec {
          name = "ipmi-fan";
          nativeBuildInputs = [pkgs.pkg-config];
          buildInputs = with pkgs; [
            clang
            llvmPackages.bintools
            cargo
            rustc
            clippy
            rustfmt
            freeipmi
          ];

          # https://github.com/rust-lang/rust-bindgen#environment-variables
          LIBCLANG_PATH = pkgs.lib.makeLibraryPath [pkgs.llvmPackages_latest.libclang.lib];

          # Add precompiled library to rustc search path
          RUSTFLAGS = builtins.map (a: ''-L ${a}/lib'') [
            # add libraries here (e.g. pkgs.libvmi)
          ];

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (buildInputs ++ nativeBuildInputs);

          # Add glibc, clang, glib, and other headers to bindgen search path
          BINDGEN_EXTRA_CLANG_ARGS =
            # Includes normal include path
            (builtins.map (a: ''-I"${a}/include"'') [
              # add dev libraries here (e.g. pkgs.libvmi.dev)
              pkgs.glibc.dev
            ])
            # Includes with special directory paths
            ++ [
              ''-I"${pkgs.llvmPackages_latest.libclang.lib}/lib/clang/${pkgs.llvmPackages_latest.libclang.version}/include"''
              ''-I"${pkgs.glib.dev}/include/glib-2.0"''
              ''-I${pkgs.glib.out}/lib/glib-2.0/include/''
            ];
        };
      }
    )
    // {
      # Import the NixOS module from a separate file
      nixosModules.default = import ./nix/module.nix;

      # Import the overlay from a separate file
      overlays.default = import ./nix/overlay.nix;
    };
}

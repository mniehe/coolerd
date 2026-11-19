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
          lib.nixosSystem {
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
          };

        # Each host names the packages whose bin directory must be on the unit's PATH
        hosts = {
          default = {
            nixos = evalHost {};
            expected = p: [p.freeipmi];
          };
          extra = {
            nixos = evalHost ({pkgs, ...}: {
              services.ipmi-fan-control.extraPackages = [pkgs.hello];
            });
            expected = p: [p.freeipmi p.hello];
          };
          drives = {
            nixos = evalHost ({pkgs, ...}: {
              services.ipmi-fan-control = {
                enableSmartMonTools = true;
                enableHdparm = true;
                extraPackages = [pkgs.hello];
              };
            });
            expected = p: [p.freeipmi p.smartmontools p.hdparm p.hello];
          };
          hwmon = {
            nixos = evalHost {
              services.ipmi-fan-control = {
                enableIpmi = false;
                hwmon.kernelModules = ["nct6775"];
                hwmon.pwmChannels.nct6798 = [2 12];
                settings.zones = [
                  {
                    fans = [
                      {
                        type = "hwmon";
                        chip = "nct6798";
                        pwm = 2;
                      }
                      {
                        type = "hwmon";
                        chip = "nct6798";
                        pwm = 12;
                      }
                    ];
                    sources = [
                      {
                        type = "hwmon";
                        chip = "k10temp";
                        input = 1;
                      }
                    ];
                    steps = [
                      {
                        temp = 40;
                        dcycle = 30;
                      }
                      {
                        temp = 80;
                        dcycle = 100;
                      }
                    ];
                  }
                ];
              };
            };
            expected = p: [p.freeipmi];
          };
        };

        hasPackage = name: packages: lib.any (p: lib.getName p == name) packages;
        unit = host: host.nixos.config.systemd.services.ipmi-fan-control;
        unitText = host: host.nixos.config.systemd.units."ipmi-fan-control.service".text;

        # PATH= at the start of a value or after a space or quote, so LD_LIBRARY_PATH= does not count
        assignsPath = s: builtins.match "(.*[ \"])?PATH=.*" s != null;
        envSetsPath = host:
          lib.any (v: lib.isString v && assignsPath v)
          (lib.toList ((unit host).serviceConfig.Environment or []));
        pathLines = host:
          lib.filter (line: lib.hasPrefix "Environment=" line && assignsPath (lib.removePrefix "Environment=" line))
          (lib.splitString "\n" (unitText host));

        # environment.PATH is what the unit file sets; a forced value there bypasses the path option
        effectivePath = host: lib.splitString ":" ((unit host).environment.PATH or "");
        missingFromPath = host:
          lib.filter (p: !lib.elem (lib.makeBinPath [p]) (effectivePath host))
          (host.expected host.nixos.pkgs);

        hwmonConfig = hosts.hwmon.nixos.config;
        accessScript = hwmonConfig.services.ipmi-fan-control.hwmon.accessScript;
        # The file each grant line of the access helper names, in order
        grantedFiles = lib.concatMap (line: let
          m = builtins.match "[[:space:]]*grant \"\\$device/(pwm[0-9]+(_enable)?)\"" line;
        in
          lib.optional (m != null) (lib.head m))
        (lib.splitString "\n" accessScript);
        execStartPre = host: lib.toList ((unit host).serviceConfig.ExecStartPre or []);

        hostFailures = name: host:
          map (p: "${name}: ${lib.getName p} is not on the effective unit PATH") (missingFromPath host)
          ++ lib.optional (envSetsPath host)
          "${name}: serviceConfig.Environment sets PATH"
          ++ lib.optional (lib.length (pathLines host) != 1)
          "${name}: the unit file does not assign PATH exactly once";

        moduleFailures =
          lib.concatLists (lib.mapAttrsToList hostFailures hosts)
          ++ lib.optional (!hasPackage "hello" (unit hosts.extra).path)
          "extraPackages: hello is not in the unit path"
          ++ lib.optional (!hasPackage "freeipmi" (unit hosts.default).path)
          "default: freeipmi is not in the unit path"
          ++ lib.optional (hosts.default.nixos.config.services.ipmi-fan-control.extraPackages != [])
          "default: extraPackages is not empty"
          ++ lib.optional (!lib.elem "nct6775" hwmonConfig.boot.kernelModules)
          "hwmon: boot.kernelModules does not contain nct6775"
          ++ lib.optional (!lib.hasInfix "find_chip ${lib.escapeShellArg "nct6798"}\n" accessScript || grantedFiles != ["pwm2" "pwm2_enable" "pwm12" "pwm12_enable"])
          "hwmon: the access helper does not grant exactly pwm2, pwm2_enable, pwm12 and pwm12_enable of nct6798"
          ++ lib.optional (!lib.any (c: lib.hasPrefix "+" c && lib.hasSuffix "-ipmi-fan-control-hwmon-access" c) (execStartPre hosts.hwmon))
          "hwmon: ExecStartPre does not run the access helper with the + prefix"
          ++ lib.optional (lib.hasInfix "KERNEL==\"ipmi" hwmonConfig.services.udev.extraRules || lib.elem "ipmi" hwmonConfig.users.users.ipmi-fan-control.extraGroups || hwmonConfig.users.groups ? ipmi || lib.elem "ipmi_si" hwmonConfig.boot.kernelModules)
          "hwmon: an IPMI udev rule, ipmi group or ipmi_si kernel module is present with enableIpmi = false";
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

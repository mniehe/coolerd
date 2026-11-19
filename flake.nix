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
        # The file each add_target line of the access helper names, in order
        grantedFiles = lib.concatMap (line: let
          m = builtins.match "[[:space:]]*add_target \"\\$device/(pwm[0-9]+(_enable)?)\"" line;
        in
          lib.optional (m != null) (lib.head m))
        (lib.splitString "\n" accessScript);
        execStartPre = host: lib.toList ((unit host).serviceConfig.ExecStartPre or []);
        # The helper with the + prefix, then the hwmon root and the service group
        runsAccessHelper = host: command: let
          words = lib.splitString " " command;
        in
          lib.length words
          == 3
          && lib.hasPrefix "+" (lib.elemAt words 0)
          && lib.hasSuffix "-ipmi-fan-control-hwmon-access" (lib.elemAt words 0)
          && lib.elemAt words 1 == "/sys/class/hwmon"
          && lib.elemAt words 2 == lib.escapeShellArg (unit host).serviceConfig.Group;
        accessHelper = pkgs.writeShellScript "hwmon-access-helper" accessScript;

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
          ++ lib.optional (!lib.any (runsAccessHelper hosts.hwmon) (execStartPre hosts.hwmon))
          "hwmon: ExecStartPre does not run the access helper with the + prefix on /sys/class/hwmon and the service group"
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

          # Runs the hwmon host's access helper against fake hwmon trees; never /sys
          hwmon-access = pkgs.runCommand "hwmon-access" {} ''
            helper=${accessHelper}
            group=$(id -g)
            failures=0
            umask 022

            fail() {
              echo "case $label: FAIL: $1" >&2
              failures=$((failures + 1))
            }

            start_case() {
              label="$1"
              root="$TMPDIR/$1"
              failures_before=$failures
              mkdir -p "$root"
            }

            end_case() {
              if [ "$failures" -eq "$failures_before" ]; then
                echo "case $label: pass" >&2
              fi
            }

            make_pwm_files() {
              mkdir -p "$1"
              for file in pwm1 pwm2 pwm2_enable pwm3 pwm12 pwm12_enable; do
                echo 0 > "$1/$file"
                chmod 644 "$1/$file"
              done
            }

            # A hwmon entry whose name file holds $2 with backslash escapes expanded
            make_chip() {
              make_pwm_files "$1"
              printf '%b' "$2" > "$1/name"
            }

            run_helper() {
              if "$helper" "$@" 2> "$TMPDIR/stderr"; then
                status=0
              else
                status=$?
              fi
              echo "case $label: exit $status: $(tr '\n' ' ' < "$TMPDIR/stderr")" >&2
            }

            expect_success() {
              if [ "$status" -ne 0 ]; then
                fail "expected exit 0, got $status"
              fi
            }

            expect_failure() {
              if [ "$status" -eq 0 ]; then
                fail "expected a non-zero exit, got 0"
              fi
            }

            expect_mode() {
              actual=$(stat -c %a "$1")
              if [ "$actual" != "$2" ]; then
                fail "$1 has mode $actual, expected $2"
              fi
            }

            # Every pwm file under $1 still has mode 644
            expect_unchanged() {
              changed=$(find "$1" -type f -name 'pwm*' ! -perm 644)
              if [ -n "$changed" ]; then
                fail "modes changed: $changed"
              fi
            }

            expect_no_other_write() {
              writable=$(find "$1" ! -type l -perm -0002)
              if [ -n "$writable" ]; then
                fail "writable by others: $writable"
              fi
            }

            # The four listed files of nct6798 in hwmon0 gain group write, nothing else changes
            expect_grants() {
              run_helper "$root" "$group"
              expect_success
              for file in pwm2 pwm2_enable pwm12 pwm12_enable; do
                expect_mode "$root/hwmon0/$file" 664
              done
              expect_mode "$root/hwmon0/pwm1" 644
              expect_mode "$root/hwmon0/pwm3" 644
              expect_unchanged "$root/hwmon1"
              expect_no_other_write "$root"
            }

            start_case a
            make_chip "$root/hwmon0" 'nct6798'
            make_chip "$root/hwmon1" 'k10temp\n'
            expect_grants
            end_case

            start_case b
            make_chip "$root/hwmon0" 'nct6798\n'
            make_chip "$root/hwmon1" 'nct6798\n'
            run_helper "$root" "$group"
            expect_failure
            expect_unchanged "$root"
            end_case

            start_case c-no-match
            make_chip "$root/hwmon0" 'k10temp\n'
            run_helper "$root" "$group"
            expect_failure
            expect_unchanged "$root"
            end_case

            start_case c-empty
            run_helper "$root" "$group"
            expect_failure
            end_case

            start_case d
            make_chip "$root/hwmon0" 'nct6798\n'
            rm "$root/hwmon0/pwm12_enable"
            run_helper "$root" "$group"
            expect_failure
            expect_mode "$root/hwmon0/pwm2" 644
            expect_unchanged "$root"
            end_case

            start_case e
            make_chip "$root/hwmon0" 'nct6798\n'
            echo 0 > "$TMPDIR/e-outside"
            chmod 644 "$TMPDIR/e-outside"
            rm "$root/hwmon0/pwm2"
            ln -s "$TMPDIR/e-outside" "$root/hwmon0/pwm2"
            run_helper "$root" "$group"
            expect_failure
            expect_mode "$TMPDIR/e-outside" 644
            expect_unchanged "$root"
            end_case

            start_case f
            make_chip "$root/hwmon0" 'nct6798\n'
            rm "$root/hwmon0/pwm2"
            mkdir -m 755 "$root/hwmon0/pwm2"
            run_helper "$root" "$group"
            expect_failure
            expect_mode "$root/hwmon0/pwm2" 755
            expect_unchanged "$root"
            end_case

            start_case g
            if [ "$(id -u)" -eq 0 ]; then
              echo "case g: skipped, the build runs as root and can read a mode 000 file" >&2
            else
              make_chip "$root/hwmon0" 'nct6798\n'
              make_chip "$root/hwmon1" 'k10temp\n'
              chmod 000 "$root/hwmon1/name"
              run_helper "$root" "$group"
              expect_failure
              expect_unchanged "$root"
              end_case
            fi

            start_case h
            make_chip "$root/hwmon0" 'nct6798'
            make_chip "$root/hwmon1" 'k10temp\n'
            make_pwm_files "$root/hwmon2"
            expect_grants
            expect_unchanged "$root/hwmon2"
            end_case

            start_case i-trimmed-name
            make_chip "$root/hwmon0" ' \tnct6798\t \n'
            make_chip "$root/hwmon1" 'k10temp\n'
            expect_grants
            end_case

            start_case j-multi-line-name
            make_chip "$root/hwmon0" 'nct6798\nnct6798\n'
            run_helper "$root" "$group"
            expect_failure
            expect_unchanged "$root"
            end_case

            start_case u-usage
            make_chip "$root/hwmon0" 'nct6798\n'
            run_helper
            if [ "$status" -ne 2 ]; then
              fail "no arguments: expected exit 2, got $status"
            fi
            run_helper "$root" "$group" extra
            if [ "$status" -ne 2 ]; then
              fail "three arguments: expected exit 2, got $status"
            fi
            expect_unchanged "$root"
            end_case

            if [ "$failures" -ne 0 ]; then
              echo "$failures failed assertions" >&2
              exit 1
            fi
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

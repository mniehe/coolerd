{
  description = "coolerd";

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

        # Evaluate a minimal host with the module and the given modules, which
        # must enable the service themselves
        evalModules = modules:
          lib.nixosSystem {
            modules =
              [
                self.nixosModules.default
                {
                  nixpkgs.hostPlatform = system;
                  nixpkgs.overlays = [self.overlays.default];
                  fileSystems."/".device = "/dev/null";
                  boot.loader.grub.enable = false;
                }
              ]
              ++ modules;
          };
        # Evaluate a minimal host that enables the service, plus one extra module
        evalHost = extraModule: evalModules [{services.coolerd.enable = true;} extraModule];

        # Settings that the migration host sets on the old option path
        migrationSettings = {
          log_level = "debug";
          zones = [
            {
              fans = [
                {
                  type = "hwmon";
                  chip = "nct6798";
                  pwm = 2;
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
              ];
            }
          ];
        };
        # Differs from migrationSettings in a shared key and an extra key, so a
        # merge of the two cannot equal migrationSettings
        migrationRival = {
          log_level = "info";
          sessions = {};
        };
        # A value other than the default for every other public leaf except
        # package, so an old path that sets nothing changes what the host builds
        migrationLeaves = pkgs: {
          enableIpmi = false;
          enableSmartMonTools = true;
          enableHdparm = true;
          extraPackages = [pkgs.hello];
          hwmon.kernelModules = ["nct6775"];
          hwmon.pwmChannels.nct6798 = [2];
        };
        migrationExpectedPath = p: [p.freeipmi p.smartmontools p.hdparm p.hello];

        # Each host names the packages whose bin directory must be on the unit's PATH
        hosts = {
          default = {
            nixos = evalHost {};
            expected = p: [p.freeipmi];
          };
          extra = {
            nixos = evalHost ({pkgs, ...}: {
              services.coolerd.extraPackages = [pkgs.hello];
            });
            expected = p: [p.freeipmi p.hello];
          };
          drives = {
            nixos = evalHost ({pkgs, ...}: {
              services.coolerd = {
                enableSmartMonTools = true;
                enableHdparm = true;
                extraPackages = [pkgs.hello];
              };
            });
            expected = p: [p.freeipmi p.smartmontools p.hdparm p.hello];
          };
          hwmon = {
            nixos = evalHost {
              services.coolerd = {
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
          # Old option paths, as a configuration written before the rename
          # sets them. The normal-priority new-path definitions compete with
          # them, so mkForce on an old path must carry over to win.
          migration = {
            nixos = evalModules [
              {services.coolerd.enable = false;}
              {services.coolerd.settings = migrationRival;}
              {services.ipmi-fan-control.enable = lib.mkForce true;}
              {services.ipmi-fan-control.settings = lib.mkForce migrationSettings;}
              ({pkgs, ...}: {services.ipmi-fan-control = migrationLeaves pkgs;})
            ];
            expected = migrationExpectedPath;
          };
          # The migration host's values through the new option paths
          migrationTwin = {
            nixos = evalHost ({pkgs, ...}: {
              services.coolerd = migrationLeaves pkgs // {settings = migrationSettings;};
            });
            expected = migrationExpectedPath;
          };
        };

        # A host whose chip name the module must reject. Only its assertions are
        # read: its toplevel and unit would fail or drop the chip.
        badChipConfig =
          (evalHost {
            services.coolerd.hwmon.pwmChannels."nct*" = [2];
          })
          .config;
        rejectsBadChip =
          lib.any (a: !a.assertion && lib.hasInfix "services.coolerd.hwmon.pwmChannels" a.message)
          badChipConfig.assertions;

        # A host with two chips. Only its access script is read, so that the
        # helper can be run against a tree where the second chip fails.
        twoChipScript =
          (evalHost {
            services.coolerd.hwmon.pwmChannels = {
              nct6798 = [2];
              it8792 = [1];
            };
          })
          .config
          .services
          .coolerd
          .hwmon
          .accessScript;

        hasPackage = name: packages: lib.any (p: lib.getName p == name) packages;
        unit = host: host.nixos.config.systemd.services.coolerd;
        unitText = host: host.nixos.config.systemd.units."coolerd.service".text;

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

        defaultConfig = hosts.default.nixos.config;
        ipmiModules = ["ipmi_devintf" "ipmi_si" "ipmi_msghandler"];
        hasIpmiModule = kernelModules: lib.any (m: lib.elem m kernelModules) ipmiModules;
        # Each rule as one whole line, so its GROUP and MODE apply to its KERNEL selector
        ipmiUdevRules = [
          "KERNEL==\"ipmi*\", GROUP=\"ipmi\", MODE=\"0660\""
          "KERNEL==\"ipmidev/*\", GROUP=\"ipmi\", MODE=\"0660\""
        ];

        hwmonConfig = hosts.hwmon.nixos.config;
        migrationConfig = hosts.migration.nixos.config;
        # The path of every option leaf under opts that is not internal
        publicLeaves = prefix: opts:
          lib.concatLists (lib.mapAttrsToList (name: value:
            if lib.isOption value
            then lib.optional (!(value.internal or false)) (prefix ++ [name])
            else publicLeaves (prefix ++ [name]) value)
          opts);
        defaultOptions = hosts.default.nixos.options.services;
        unaliasedLeaves = lib.filter (leaf: !lib.hasAttrByPath leaf defaultOptions.ipmi-fan-control) (publicLeaves [] defaultOptions.coolerd);
        # Leaves the migration host leaves unset on the old path, so their
        # forwarding goes unchecked; package is left out on purpose
        migrationOldOptions = hosts.migration.nixos.options.services.ipmi-fan-control;
        unsetOldLeaves =
          lib.filter (leaf: leaf != ["package"] && !(lib.attrByPath leaf {isDefined = false;} migrationOldOptions).isDefined)
          (publicLeaves [] defaultOptions.coolerd);
        # What the options produce on a host, for comparing the old-path and new-path hosts
        producedBy = host: let
          config = host.nixos.config;
          etc = config.environment.etc."coolerd.toml";
        in {
          "the unit file" = unitText host;
          "the service user" = {inherit (config.users.users.coolerd) group extraGroups isSystemUser home;};
          "users.groups" = lib.attrNames config.users.groups;
          "the udev rules" = config.services.udev.extraRules;
          "boot.kernelModules" = config.boot.kernelModules;
          "/etc/coolerd.toml" = {
            inherit (etc) mode user group;
            source = "${etc.source}";
          };
        };
        migrationDifferences = let
          twin = producedBy hosts.migrationTwin;
        in
          lib.attrNames (lib.filterAttrs (name: value: value != twin.${name}) (producedBy hosts.migration));
        # The config file the module would generate from the old-path settings
        migrationExpected = (pkgs.formats.toml {}).generate "coolerd-config.toml" migrationSettings;
        accessScript = hwmonConfig.services.coolerd.hwmon.accessScript;
        # The first capture of pattern on each line of script that matches it, in order
        scriptMatches = pattern: script:
          lib.concatMap (line: let
            m = builtins.match pattern line;
          in
            lib.optional (m != null) (lib.head m))
          (lib.splitString "\n" script);
        # The file each add_target line of an access helper names
        filesGranted = scriptMatches "[[:space:]]*add_target \"\\$device/(pwm[0-9]+(_enable)?)\"";
        # The chip each find_chip call of an access helper names
        chipsFound = scriptMatches "find_chip (.*)";
        grantedFiles = filesGranted accessScript;
        execStartPre = host: lib.toList ((unit host).serviceConfig.ExecStartPre or []);
        # The helper with the + prefix, then the hwmon root and the service group
        runsAccessHelper = host: command: let
          words = lib.splitString " " command;
        in
          lib.length words
          == 3
          && lib.hasPrefix "+" (lib.elemAt words 0)
          && lib.hasSuffix "-coolerd-hwmon-access" (lib.elemAt words 0)
          && lib.elemAt words 1 == "/sys/class/hwmon"
          && lib.elemAt words 2 == lib.escapeShellArg (unit host).serviceConfig.Group;
        accessHelper = pkgs.writeShellScript "hwmon-access-helper" accessScript;
        twoChipHelper = pkgs.writeShellScript "hwmon-access-helper-two-chips" twoChipScript;

        configFileAccess = host: {inherit (host.nixos.config.environment.etc."coolerd.toml") user group mode;};
        serviceOnlyAccess = {
          user = "coolerd";
          group = "coolerd";
          mode = "0600";
        };

        hostFailures = name: host:
          map (p: "${name}: ${lib.getName p} is not on the effective unit PATH") (missingFromPath host)
          ++ lib.optional (envSetsPath host)
          "${name}: serviceConfig.Environment sets PATH"
          ++ lib.optional (lib.length (pathLines host) != 1)
          "${name}: the unit file does not assign PATH exactly once"
          ++ lib.optional ((unit host).serviceConfig.User != "coolerd" || (unit host).serviceConfig.Group != "coolerd")
          "${name}: the unit does not run as the user and group coolerd"
          ++ lib.optional (!lib.hasSuffix "/bin/coolerd --config /etc/coolerd.toml" (unit host).serviceConfig.ExecStart)
          "${name}: ExecStart does not run bin/coolerd with /etc/coolerd.toml"
          ++ lib.optional (!(host.nixos.config.environment.etc ? "coolerd.toml"))
          "${name}: /etc/coolerd.toml is not defined"
          ++ lib.optional (configFileAccess host != serviceOnlyAccess)
          "${name}: /etc/coolerd.toml is not owned by coolerd:coolerd with mode 0600";

        moduleFailures =
          lib.concatLists (lib.mapAttrsToList hostFailures hosts)
          ++ lib.optional (!hasPackage "hello" (unit hosts.extra).path)
          "extraPackages: hello is not in the unit path"
          ++ lib.optional (!hasPackage "freeipmi" (unit hosts.default).path)
          "default: freeipmi is not in the unit path"
          ++ lib.optional (hosts.default.nixos.config.services.coolerd.extraPackages != [])
          "default: extraPackages is not empty"
          ++ lib.optional (!lib.all (m: lib.elem m defaultConfig.boot.kernelModules) ipmiModules)
          "default: boot.kernelModules does not contain ipmi_devintf, ipmi_si and ipmi_msghandler"
          ++ lib.optional (!(defaultConfig.users.groups ? ipmi))
          "default: the ipmi group does not exist"
          ++ lib.optional (!lib.elem "ipmi" defaultConfig.users.users.coolerd.extraGroups)
          "default: the service user is not in the ipmi group"
          ++ lib.optional (!lib.all (rule: lib.elem rule (lib.splitString "\n" defaultConfig.services.udev.extraRules)) ipmiUdevRules)
          "default: the udev rules do not give the ipmi group mode 0660 on both ipmi* and ipmidev/*"
          ++ lib.optional (execStartPre hosts.default != [])
          "default: the unit has an ExecStartPre"
          ++ lib.optional (!lib.elem "nct6775" hwmonConfig.boot.kernelModules)
          "hwmon: boot.kernelModules does not contain nct6775"
          ++ lib.optional (!lib.hasInfix "find_chip ${lib.escapeShellArg "nct6798"}\n" accessScript || grantedFiles != ["pwm2" "pwm2_enable" "pwm12" "pwm12_enable"])
          "hwmon: the access helper does not grant exactly pwm2, pwm2_enable, pwm12 and pwm12_enable of nct6798"
          # hwmon-access fails the chip found last; it must be nct6798
          ++ lib.optional (chipsFound twoChipScript != map lib.escapeShellArg ["it8792" "nct6798"] || filesGranted twoChipScript != ["pwm1" "pwm1_enable" "pwm2" "pwm2_enable"])
          "two chips: the access helper does not find it8792 and then nct6798, granting pwm1 and pwm1_enable and then pwm2 and pwm2_enable"
          ++ lib.optional (!lib.any (runsAccessHelper hosts.hwmon) (execStartPre hosts.hwmon))
          "hwmon: ExecStartPre does not run the access helper with the + prefix on /sys/class/hwmon and the service group"
          ++ lib.optional (lib.hasInfix "KERNEL==\"ipmi" hwmonConfig.services.udev.extraRules || lib.elem "ipmi" hwmonConfig.users.users.coolerd.extraGroups || hwmonConfig.users.groups ? ipmi || hasIpmiModule hwmonConfig.boot.kernelModules)
          "hwmon: an IPMI udev rule, ipmi group or IPMI kernel module is present with enableIpmi = false"
          ++ lib.optional (!rejectsBadChip)
          "bad chip: no failed assertion names services.coolerd.hwmon.pwmChannels for the chip nct*"
          ++ lib.optional (migrationConfig.services.coolerd.settings != migrationSettings)
          "migration: services.coolerd.settings is not the settings set on the old path"
          ++ lib.optional (migrationConfig.systemd.services ? ipmi-fan-control || migrationConfig.environment.etc ? "ipmi-fan-control.toml")
          "migration: the unit or config file still has the old name"
          ++ map (leaf: "migration: services.ipmi-fan-control.${lib.concatStringsSep "." leaf} is not an alias") unaliasedLeaves
          ++ map (leaf: "migration: the host does not set services.ipmi-fan-control.${lib.concatStringsSep "." leaf}") unsetOldLeaves
          ++ map (name: "migration: ${name} differs from the host that sets the new option paths") migrationDifferences
          ++ lib.optional (pkgs.ipmi-fan-control.drvPath != pkgs.coolerd.drvPath)
          "overlay: ipmi-fan-control is not the coolerd derivation"
          ++ lib.optional ((pkgs.coolerd.meta.mainProgram or null) != "coolerd")
          "overlay: meta.mainProgram of coolerd is not coolerd"
          ++ lib.optional (self.packages.${system}.ipmi-fan-control.drvPath != self.packages.${system}.default.drvPath)
          "packages: ipmi-fan-control is not the default package";
      in {
        formatter = pkgs.alejandra;

        checks = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          module-eval = pkgs.runCommand "module-eval" {} ''
            ${lib.concatMapStrings (m: "echo ${lib.escapeShellArg m} >&2\n") moduleFailures}
            ${lib.optionalString (moduleFailures != []) "exit 1"}
            if ! cmp ${migrationConfig.environment.etc."coolerd.toml".source} ${migrationExpected}; then
              echo "migration: /etc/coolerd.toml does not hold the settings set on the old path" >&2
              exit 1
            fi
            touch $out
          '';

          # Runs the hwmon host's access helper against fake hwmon trees; never /sys
          hwmon-access = pkgs.runCommand "hwmon-access" {} ''
            one_chip_helper=${accessHelper}
            two_chip_helper=${twoChipHelper}
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
              helper=$one_chip_helper
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
              for file in pwm1 pwm1_enable pwm2 pwm2_enable pwm3 pwm12 pwm12_enable; do
                echo 0 > "$1/$file"
                chmod 644 "$1/$file"
              done
            }

            # A hwmon entry whose name file holds $2 with backslash escapes expanded
            make_chip() {
              make_pwm_files "$1"
              printf '%b' "$2" > "$1/name"
            }

            # As make_chip, but the device directory lies outside the hwmon root
            # and $1 is a symbolic link to it, as in /sys/class/hwmon
            make_linked_chip() {
              device="$TMPDIR/$label-devices/$(basename "$1")/hwmon"
              make_chip "$device" "$2"
              ln -s "$device" "$1"
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

            expect_stderr() {
              if ! grep -q -F -- "$1" "$TMPDIR/stderr"; then
                fail "stderr does not contain: $1"
              fi
            }

            expect_mode() {
              actual=$(stat -c %a "$1")
              if [ "$actual" != "$2" ]; then
                fail "$1 has mode $actual, expected $2"
              fi
            }

            expect_group() {
              actual=$(stat -c %g "$1")
              if [ "$actual" != "$2" ]; then
                fail "$1 has gid $actual, expected $2"
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
              writable=$(find -L "$1" ! -type l -perm -0002)
              if [ -n "$writable" ]; then
                fail "writable by others: $writable"
              fi
            }

            # The mode of every path under the hwmon root, through its symbolic
            # links, except the paths given as arguments
            other_modes() {
              excluded=()
              for target in "$@"; do
                excluded+=(! -path "$target")
              done
              find -L "$root" "''${excluded[@]}" -printf '%p %m\n' | sort
            }

            # The files given as arguments gain group write and the group, and
            # no other path under the hwmon root changes its mode
            expect_grants_to() {
              other_modes "$@" > "$TMPDIR/before"
              run_helper "$root" "$group"
              expect_success
              for target in "$@"; do
                expect_mode "$target" 664
                expect_group "$target" "$group"
              done
              other_modes "$@" > "$TMPDIR/after"
              if ! cmp -s "$TMPDIR/before" "$TMPDIR/after"; then
                fail "other modes changed: $(diff "$TMPDIR/before" "$TMPDIR/after" | tr '\n' ' ')"
              fi
              expect_no_other_write "$root"
            }

            # The four listed files of nct6798 in hwmon0 gain group write, nothing else changes
            expect_grants() {
              expect_grants_to "$root/hwmon0/pwm2" "$root/hwmon0/pwm2_enable" "$root/hwmon0/pwm12" "$root/hwmon0/pwm12_enable"
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
            expect_stderr "hwmon chip nct6798:"
            expect_unchanged "$root"
            end_case

            start_case c-no-match
            make_chip "$root/hwmon0" 'k10temp\n'
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "hwmon chip nct6798:"
            expect_unchanged "$root"
            end_case

            start_case c-empty
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "hwmon chip nct6798:"
            end_case

            start_case d
            make_chip "$root/hwmon0" 'nct6798\n'
            rm "$root/hwmon0/pwm12_enable"
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "$root/hwmon0/pwm12_enable:"
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
            expect_stderr "$root/hwmon0/pwm2:"
            expect_mode "$TMPDIR/e-outside" 644
            expect_unchanged "$root"
            end_case

            start_case f
            make_chip "$root/hwmon0" 'nct6798\n'
            rm "$root/hwmon0/pwm2"
            mkdir -m 755 "$root/hwmon0/pwm2"
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "$root/hwmon0/pwm2:"
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
              expect_stderr "$root/hwmon1/name: cannot read"
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
            expect_stderr "hwmon chip nct6798:"
            expect_unchanged "$root"
            end_case

            start_case u-usage
            make_chip "$root/hwmon0" 'nct6798\n'
            run_helper
            if [ "$status" -ne 2 ]; then
              fail "no arguments: expected exit 2, got $status"
            fi
            expect_stderr "usage:"
            run_helper "$root" "$group" extra
            if [ "$status" -ne 2 ]; then
              fail "three arguments: expected exit 2, got $status"
            fi
            expect_stderr "usage:"
            expect_unchanged "$root"
            end_case

            start_case k-unknown-group
            make_chip "$root/hwmon0" 'nct6798\n'
            run_helper "$root" no-such-group-coolerd-check
            expect_failure
            expect_stderr "no-such-group-coolerd-check"
            expect_unchanged "$root"
            end_case

            start_case n-class-symlinks
            make_linked_chip "$root/hwmon0" 'nct6798\n'
            make_linked_chip "$root/hwmon1" 'k10temp\n'
            expect_grants
            end_case

            # A name that cannot be read even by root
            start_case o-name-directory
            make_chip "$root/hwmon0" 'nct6798\n'
            make_pwm_files "$root/hwmon1"
            mkdir "$root/hwmon1/name"
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "$root/hwmon1/name: cannot read"
            expect_unchanged "$root"
            end_case

            start_case l-two-chips
            helper=$two_chip_helper
            make_chip "$root/hwmon0" 'nct6798\n'
            make_chip "$root/hwmon1" 'it8792\n'
            expect_grants_to "$root/hwmon1/pwm1" "$root/hwmon1/pwm1_enable" "$root/hwmon0/pwm2" "$root/hwmon0/pwm2_enable"
            end_case

            # nct6798, the chip the helper handles last, fails after it8792 has passed
            start_case m-two-chips-late-failure
            helper=$two_chip_helper
            make_chip "$root/hwmon0" 'nct6798\n'
            make_chip "$root/hwmon1" 'it8792\n'
            rm "$root/hwmon0/pwm2_enable"
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "$root/hwmon0/pwm2_enable:"
            expect_mode "$root/hwmon1/pwm1" 644
            expect_mode "$root/hwmon1/pwm1_enable" 644
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
          default = pkgs.coolerd;
          coolerd = pkgs.coolerd;
          ipmi-fan-control = pkgs.ipmi-fan-control;
        };

        devShells.default = pkgs.mkShell rec {
          name = "coolerd";
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

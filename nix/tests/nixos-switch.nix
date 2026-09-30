# The NixOS activation arm of `sui system rebuild`, run on a machine.
#
# sui-orchestrate's switch-to-configuration dispatch had unit tests but had
# never executed on NixOS. This VM boots a small system, then drives every
# mutating verb through sui against a changed and an unchanged toplevel and
# checks the machine, not sui's report of it:
#
#   switch   /run/current-system moves, a new profile generation appears, the
#            changed unit restarts, the bootloader hook runs
#   boot     the profile moves and the bootloader hook runs (and sees
#            NIXOS_INSTALL_BOOTLOADER); the running system does not change
#   test     the running system moves and the unit restarts; the profile does
#            NOT move
#   rollback the profile returns to the previous generation without minting a
#            new one, and that generation is activated
#
# The toplevels are built by the test (the base system and its `changed`
# specialisation), so this proves ACTIVATION only. Evaluating a NixOS flake
# with sui inside the VM is a separate claim, measured by the flip probe.
{ pkgs, sui }:
pkgs.testers.runNixOSTest {
  name = "sui-nixos-switch";

  nodes.machine =
    { lib, pkgs, ... }:
    {
      environment.systemPackages = [ sui ];

      # A bootloader that records its calls: switch-to-configuration hands the
      # hook the toplevel on `switch` and `boot`, never on `test`.
      boot.loader.external = {
        enable = true;
        installHook = pkgs.writeShellScript "record-bootloader-install" ''
          echo "$1 ''${NIXOS_INSTALL_BOOTLOADER:-0}" >> /var/lib/bootloader-installs
        '';
      };

      # The unit whose definition differs between the two toplevels.
      systemd.services.probe = {
        wantedBy = [ "multi-user.target" ];
        environment.MARKER = "base";
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${pkgs.coreutils}/bin/true";
        };
      };

      specialisation.changed.configuration = {
        systemd.services.probe.environment.MARKER = lib.mkForce "changed";
      };
    };

  testScript =
    { nodes, ... }:
    ''
      base = "${nodes.machine.system.build.toplevel}"
      profile = "/nix/var/nix/profiles/system"

      def current():
          return machine.succeed("readlink -f /run/current-system").strip()

      def profile_target():
          return machine.succeed(f"readlink -f {profile}").strip()

      def generation():
          return machine.succeed(f"readlink {profile}").strip().rsplit("/", 1)[-1]

      def invocation():
          return machine.succeed("systemctl show -P InvocationID probe").strip()

      def marker():
          env = machine.succeed("systemctl show -P Environment probe").split()
          return next((kv for kv in env if kv.startswith("MARKER=")), "")

      def installs():
          return machine.succeed("cat /var/lib/bootloader-installs 2>/dev/null || true").strip().splitlines()

      machine.wait_for_unit("multi-user.target")
      # The specialisation's own store path, as switch-to-configuration and
      # /run/current-system name it (the specialisation/ entry is a symlink).
      changed = machine.succeed(f"readlink -f {base}/specialisation/changed").strip()
      machine.wait_for_unit("probe.service")
      assert current() == base, f"booted {current()}"
      assert marker() == "MARKER=base", marker()

      with subtest("a path without switch-to-configuration is refused before anything mutates"):
          out = machine.fail("sui system rebuild switch --toplevel /var/empty 2>&1")
          assert "not a toplevel" in out, out
          machine.fail(f"test -e {profile}")
          assert current() == base

      with subtest("switch: new generation, running system moves, changed unit restarts"):
          before = invocation()
          machine.succeed(f"sui system rebuild switch --toplevel {changed} >&2")
          assert current() == changed, current()
          assert generation() == "system-1-link", generation()
          assert profile_target() == changed, profile_target()
          assert invocation() != before, "probe.service was not restarted"
          assert marker() == "MARKER=changed", marker()
          assert installs() == [f"{changed} 0"], installs()

      with subtest("boot: profile moves and the bootloader is installed; nothing activates"):
          before = invocation()
          machine.succeed(f"NIXOS_INSTALL_BOOTLOADER=1 sui system rebuild boot --toplevel {base} >&2")
          assert generation() == "system-2-link", generation()
          assert profile_target() == base, profile_target()
          assert current() == changed, current()
          assert invocation() == before, "boot must not restart units"
          assert installs()[-1] == f"{base} 1", installs()

      with subtest("test: running system moves, profile does not"):
          n = len(installs())
          machine.succeed(f"sui system rebuild test --toplevel {base} >&2")
          assert current() == base, current()
          assert generation() == "system-2-link", generation()
          machine.fail(f"test -e {profile}-3-link")
          assert marker() == "MARKER=base", marker()
          assert len(installs()) == n, "test must not install the bootloader"

      with subtest("rollback: previous generation, no new one, and it is activated"):
          machine.succeed("sui system rollback >&2")
          assert generation() == "system-1-link", generation()
          machine.fail(f"test -e {profile}-3-link")
          assert current() == changed, current()
          assert marker() == "MARKER=changed", marker()
    '';
}

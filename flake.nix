{
  description = "backup-home — daily restic + rclone backup of $HOME on macOS, with optional systeml/launchd timer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    {
      homeManagerModules.default = ./nix/home-manager.nix;
      homeManagerModules.backup-home = ./nix/home-manager.nix;
    };
}

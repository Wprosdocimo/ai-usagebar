{
  description = "AI plan usage for Waybar, Omarchy, and the terminal";

  # Keep updates on the maintained final release branch for x86_64-darwin.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };
      packageFor = system: (pkgsFor system).callPackage ./nix/package.nix { };
    in
    {
      packages = forAllSystems (
        system:
        let
          package = packageFor system;
        in
        {
          default = package;
          ai-usagebar = package;
        }
      );

      checks = forAllSystems (system: {
        default = self.packages.${system}.default;
      });
    };
}

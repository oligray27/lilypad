// Images imported by the frontend become URLs served by Decky (see @decky/rollup's importAssets).
declare module "*.png" {
  const url: string;
  export default url;
}

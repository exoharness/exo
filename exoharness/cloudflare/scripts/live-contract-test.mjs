import { runContracts } from "./contract-test.mjs";
import { base } from "./live-api.mjs";

console.log(await runContracts(`${base}/exo`, process.env.EXO_TOKEN));

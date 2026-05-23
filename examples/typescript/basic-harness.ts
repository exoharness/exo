import { defineHarness } from "@exo/harness";

import { runResponsesHarnessTurn } from "./turn-loop";

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context);
  },
});

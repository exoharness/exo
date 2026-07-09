export default {
  name: 'walk_path_until_event',
  description: 'Follow a planned button path step-by-step, logging map/coords/facing after each step and stopping early if a battle starts or the map changes. Useful for executing known overworld routes without overshooting into grass or past warps.',
  parameters: {
    type: 'object',
    properties: {
      buttons: {
        type: 'array',
        items: { type: 'string', enum: ['a','b','start','select','up','down','left','right'] },
        minItems: 1,
        maxItems: 100
      },
      hold_frames: { type: 'number', minimum: 1, maximum: 120, default: 10 },
      wait_frames: { type: 'number', minimum: 0, maximum: 600, default: 45 },
      stop_on_battle: { type: 'boolean', default: true },
      stop_on_map_change: { type: 'boolean', default: true }
    },
    required: ['buttons']
  },
  async execute(args, ctx) {
    const hold = args.hold_frames ?? 10;
    const wait = args.wait_frames ?? 45;
    const stopOnBattle = args.stop_on_battle ?? true;
    const stopOnMapChange = args.stop_on_map_change ?? true;

    const start = await ctx.emulator.frame();
    const startState = start.state || {};
    const startMap = startState.map_id;
    const log = [];

    for (let i = 0; i < args.buttons.length; i++) {
      const button = args.buttons[i];
      const res = await ctx.emulator.press([button], hold, wait);
      const s = res.state || {};
      const row = {
        step: i + 1,
        button,
        map_id: s.map_id,
        map: s.map_name,
        x: s.x,
        y: s.y,
        facing: s.facing,
        battle: !!s.battle
      };
      log.push(row);
      ctx.log(JSON.stringify(row));

      if (stopOnBattle && s.battle) {
        return {
          stopped: true,
          reason: 'battle',
          start: { map_id: startMap, map: startState.map_name, x: startState.x, y: startState.y, facing: startState.facing },
          steps_executed: i + 1,
          log,
          final_state: s,
          screen_hash: res.screen_hash
        };
      }

      if (stopOnMapChange && startMap != null && s.map_id !== startMap) {
        return {
          stopped: true,
          reason: 'map_change',
          start: { map_id: startMap, map: startState.map_name, x: startState.x, y: startState.y, facing: startState.facing },
          steps_executed: i + 1,
          log,
          final_state: s,
          screen_hash: res.screen_hash
        };
      }
    }

    const finalState = log.length ? log[log.length - 1] : null;
    return {
      stopped: false,
      reason: 'completed',
      start: { map_id: startMap, map: startState.map_name, x: startState.x, y: startState.y, facing: startState.facing },
      steps_executed: log.length,
      log,
      final_state: finalState
    };
  }
};
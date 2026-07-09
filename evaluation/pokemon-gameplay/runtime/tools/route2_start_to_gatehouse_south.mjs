export default {
  name: 'route2_start_to_gatehouse_south',
  description: 'From the standard Viridian north-exit landing on Route 2 around (7,71), follow the verified stepwise route to the south door of the Viridian Forest gatehouse, stopping early if a battle starts or the map changes.',
  parameters: {
    type: 'object',
    properties: {
      hold_frames: { type: 'number', default: 10 },
      wait_frames: { type: 'number', default: 45 },
      stop_on_battle: { type: 'boolean', default: true }
    },
    additionalProperties: false
  },
  async execute(args, ctx) {
    const hold = args.hold_frames ?? 10;
    const wait = args.wait_frames ?? 45;
    const stopOnBattle = args.stop_on_battle ?? true;

    const route = [
      ...Array(14).fill('up'),    // (7,71) -> (7,57)
      ...Array(4).fill('left'),   // -> (3,57)
      ...Array(9).fill('up'),     // -> (3,48)
      ...Array(5).fill('right'),  // -> (8,48)
      ...Array(2).fill('up'),     // -> (8,46)
      ...Array(5).fill('left'),   // -> (3,46)
      ...Array(2).fill('up')      // -> (3,44)
    ];

    const log = [];
    let last = null;
    for (let i = 0; i < route.length; i++) {
      const button = route[i];
      const res = await ctx.emulator.press([button], hold, wait);
      last = res;
      const s = res.state || {};
      log.push({
        step: i + 1,
        button,
        map: s.map_name || s.map || null,
        x: s.x ?? null,
        y: s.y ?? null,
        battle: s.battle_type || s.battle || null
      });
      if (stopOnBattle && s.battle_type && s.battle_type !== 'none') {
        ctx.log(`Stopped on battle after step ${i + 1}`);
        return { stopped: 'battle', log, state: s };
      }
      if (s.map_name && s.map_name !== 'Route 2') {
        ctx.log(`Stopped on map change to ${s.map_name} after step ${i + 1}`);
        return { stopped: 'map_change', log, state: s };
      }
    }
    return { stopped: 'completed', log, state: last?.state || null };
  }
};
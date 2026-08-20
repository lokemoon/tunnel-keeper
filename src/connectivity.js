'use strict';

// Announces online <-> offline transitions, with two rules that matter more than the
// announcement itself:
//
//   1. Only when the user cannot already see it. If the window is the active window,
//      the badge and the banner are the notification; a toast on top would be telling
//      them something they are looking at.
//   2. Only once the new state has held. Connectivity flaps, and a two-second blip
//      would otherwise fire "lost" then "restored" back to back.
//
// Dependencies are injected so this can be exercised without a real window, a real
// network, or real OS notifications.

const SETTLE_MS = 4000;

function create({ isForeground, onLost, onRestored, getState, settleMs = SETTLE_MS }) {
  let lastOnline = null; // null until the first observation
  let settling = null; // { to, timer }

  function cancel() {
    if (settling) {
      clearTimeout(settling.timer);
      settling = null;
    }
  }

  function observe(state) {
    // With no credentials loaded there is no connection to have an opinion about.
    if (!state.creds.loaded) {
      lastOnline = null;
      cancel();
      return;
    }

    const now = !!state.creds.online;

    // First observation after loading: adopt it silently rather than announcing it.
    if (lastOnline === null) {
      lastOnline = now;
      return;
    }
    if (now === lastOnline) return;
    if (settling && settling.to === now) return; // already waiting on this same flip

    cancel();
    settling = {
      to: now,
      timer: setTimeout(() => {
        settling = null;
        const settled = getState();
        // Flipped back while we waited, so there is nothing worth telling anyone.
        if (!settled.creds.loaded || !!settled.creds.online !== now) return;

        lastOnline = now;
        if (isForeground()) return;

        const live = settled.tunnels.filter(
          (t) => t.status === 'open' || t.status === 'starting'
        ).length;

        if (now) onRestored({ live });
        else onLost({ reason: settled.creds.offlineReason || 'AWS cannot be reached.', live });
      }, settleMs),
    };
  }

  return { observe, cancel, state: () => ({ lastOnline, settlingTo: settling && settling.to }) };
}

module.exports = { create, SETTLE_MS };

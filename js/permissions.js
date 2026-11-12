// oam:permissions - Deno-shaped permission query/revoke surface.
//
// Registers as `oam:permissions` via __oamNode.factories, same pattern as
// oam:test.  The Rust side exposes __oam.node.permissionsQuery(name, path)
// which returns {state: "granted" | "denied"}.
//
// SNAPSHOT CONSTRAINT: top level only registers the factory; nothing here
// may touch natives, timers, or Date directly.
"use strict";
(() => {
  globalThis.__oamNode.factories["oam:permissions"] = () => {
    const node = globalThis.__oam.node;

    function assertDescriptor(desc) {
      if (typeof desc !== "object" || desc === null || typeof desc.name !== "string") {
        throw new TypeError(
          'Permission descriptor must be an object with a string "name" property'
        );
      }
    }

    /**
     * Normalise a PermissionDescriptor to {name, target}, where target is
     * the one string the Rust side matches against that permission's
     * allow-list, or null for an unscoped query.
     *
     * oam's own keys are "path" and "url". Deno spells the scope per
     * permission -- "host" for net, "variable" for env, "command" for a
     * subprocess -- and those are read first for the permission they belong
     * to. Dropping one would turn a scoped question into the unscoped one,
     * which answers "granted" for any non-empty allow-list.
     */
    function normalise(desc) {
      assertDescriptor(desc);
      const str = (v) => (typeof v === "string" ? v : undefined);
      let scoped;
      switch (desc.name) {
        case "net":   scoped = str(desc.host);     break;
        case "env":   scoped = str(desc.variable); break;
        case "child": scoped = str(desc.command);  break;
        default:      scoped = undefined;
      }
      return {
        name: desc.name,
        target: scoped ?? str(desc.path) ?? str(desc.url) ?? null,
      };
    }

    const permissions = {
      /**
       * query(descriptor) -> PermissionStatus
       * Compatible with Deno.permissions.query({name: "read", path: "/tmp"}).
       */
      query(desc) {
        const { name, target } = normalise(desc);
        const result = node.permissionsQuery(name, target);
        // Return a PermissionStatus-shaped object (state + eventTarget stub).
        return Promise.resolve({
          state: result.state,
          onchange: null,
          toString() { return `[PermissionStatus: ${this.state}]`; },
        });
      },

      /**
       * request(descriptor) -> PermissionStatus
       * In oam (no interactive UI), request() always returns the current
       * state (same as query).  Dynamic prompts are out of scope for M2.
       */
      request(desc) {
        return permissions.query(desc);
      },

      /**
       * revoke(descriptor) -> PermissionStatus
       * In oam M2, revoke() always returns {state: "denied"} for recognised
       * permissions; the runtime does not yet support dynamic revocation.
       * Included for API shape parity with Deno.
       */
      revoke(desc) {
        assertDescriptor(desc);
        return Promise.resolve({
          state: "denied",
          onchange: null,
          toString() { return "[PermissionStatus: denied]"; },
        });
      },
    };

    return { permissions };
  };
})();

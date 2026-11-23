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
     *
     * For the same reason a scope key that is present but not a string (a
     * URL object, a number) is refused with a TypeError rather than skipped:
     * only a descriptor with no scope at all asks the unscoped question.
     */
    function normalise(desc) {
      assertDescriptor(desc);
      let scopeKey;
      switch (desc.name) {
        case "net":   scopeKey = "host";     break;
        case "env":   scopeKey = "variable"; break;
        case "child": scopeKey = "command";  break;
        default:      scopeKey = undefined;
      }
      const keys = scopeKey === undefined ? ["path", "url"] : [scopeKey, "path", "url"];
      for (const key of keys) {
        const value = desc[key];
        if (value === undefined || value === null) continue;
        if (typeof value !== "string") {
          throw new TypeError(
            `Permission descriptor "${key}" must be a string, got ${
              typeof value === "object" ? "an object" : typeof value
            }`
          );
        }
        return { name: desc.name, target: value };
      }
      return { name: desc.name, target: null };
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

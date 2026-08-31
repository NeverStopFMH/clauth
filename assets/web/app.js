// clauth dashboard — Alpine.js component. No build step: this file is
// embedded verbatim into the binary and served as-is.

const TABS = [
  { id: "overview", label: "Overview" },
  { id: "usage", label: "Usage" },
  { id: "tokens", label: "Tokens" },
  { id: "setup", label: "Setup" },
  { id: "fallback", label: "Fallback" },
  { id: "config", label: "Config" },
  { id: "status", label: "Status" },
  { id: "plugin", label: "Plugin" },
];

const STATUS_POLL_MS = 3000;

function fmtPct(v) {
  return v === null || v === undefined ? "—" : `${Math.round(v)}%`;
}

function fmtReset(iso) {
  if (!iso) return "";
  const ms = new Date(iso).getTime() - Date.now();
  if (Number.isNaN(ms) || ms <= 0) return "";
  const mins = Math.round(ms / 60000);
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  const remMins = mins % 60;
  if (hours < 24) return `${hours}h ${remMins}m`;
  const days = Math.floor(hours / 24);
  return `${days}d ${hours % 24}h`;
}

function windowFor(profile, label) {
  return (profile.windows || []).find((w) => w.label === label) || null;
}

function gaugeClass(pct, threshold) {
  if (pct === null || pct === undefined) return "";
  const ratio = threshold ? pct / threshold : 0;
  if (ratio >= 1) return "danger";
  if (ratio >= 0.85) return "warn";
  return "";
}

document.addEventListener("alpine:init", () => {
  Alpine.data("dashboard", () => ({
    tabs: TABS,
    tab: "overview",
    status: null,
    statusError: null,
    fallbackData: null,
    fallbackSelected: null,
    fallbackAdding: false,
    fieldErrors: {},
    toasts: [],
    _toastId: 0,
    _pollTimer: null,
    _dragIndex: null,

    init() {
      this.fetchStatus();
      this._pollTimer = setInterval(() => {
        if (document.visibilityState === "visible") this.fetchStatus();
      }, STATUS_POLL_MS);
      document.addEventListener("visibilitychange", () => {
        if (document.visibilityState === "visible") this.fetchStatus();
      });
    },

    selectTab(id) {
      this.tab = id;
      if (id === "fallback") this.loadFallback();
    },

    async fetchStatus() {
      try {
        const res = await fetch("/api/status");
        if (!res.ok) {
          this.statusError = res.status === 503 ? "starting up…" : `error ${res.status}`;
          return;
        }
        this.status = await res.json();
        this.statusError = null;
      } catch {
        this.statusError = "unreachable";
      }
    },

    pushToast(text) {
      const id = ++this._toastId;
      this.toasts.push({ id, text });
      setTimeout(() => {
        this.toasts = this.toasts.filter((t) => t.id !== id);
      }, 3000);
    },

    fmtPct,
    fmtReset,
    windowFor,
    gaugeClass,

    // ---- Overview ----

    statusMarker(p) {
      if (p.auth_status === "broken") return { glyph: "×", cls: "danger" };
      if (p.auth_status === "expiring") return { glyph: "⊘", cls: "danger" };
      if (p.active) return { glyph: "●", cls: "active" };
      return { glyph: "", cls: "" };
    },

    async switchProfile(name) {
      try {
        const res = await fetch("/api/profiles/switch", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ name }),
        });
        if (!res.ok) {
          const body = await res.json().catch(() => ({}));
          this.pushToast(`switch failed: ${body.error || res.status}`);
          return;
        }
        this.pushToast(`switched to ${name}`);
        this.fetchStatus();
      } catch (e) {
        this.pushToast(`switch failed: ${e}`);
      }
    },

    chainNodes() {
      if (!this.status) return [];
      const chain = this.status.profiles
        .filter((p) => p.fallback)
        .sort((a, b) => a.fallback.position - b.fallback.position);
      return chain.map((p) => {
        const w5h = windowFor(p, "5h");
        const pct = w5h ? w5h.utilization_pct : null;
        let state = "";
        if (p.fallback.armed) state = "active";
        else if (p.auth_status === "broken") state = "blocked";
        return { profile: p, pct, threshold: p.fallback.threshold, state };
      });
    },

    // ---- Fallback ----

    async loadFallback() {
      try {
        const res = await fetch("/api/fallback");
        if (!res.ok) return;
        this.fallbackData = await res.json();
        if (!this.fallbackSelected && this.fallbackData.chain.length > 0) {
          this.fallbackSelected = this.fallbackData.chain[0].name;
        }
      } catch {
        // left as-is; the next poll-driven refresh will retry
      }
    },

    selectedFallbackMember() {
      if (!this.fallbackData || !this.fallbackSelected) return null;
      return this.fallbackData.chain.find((m) => m.name === this.fallbackSelected) || null;
    },

    fieldError(name, field) {
      return this.fieldErrors[`${name}.${field}`] || null;
    },

    async patchMember(name, field, value) {
      const key = `${name}.${field}`;
      try {
        const res = await fetch(`/api/profiles/${encodeURIComponent(name)}/fallback`, {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ [field]: value }),
        });
        if (!res.ok) {
          const body = await res.json().catch(() => ({}));
          this.fieldErrors = { ...this.fieldErrors, [key]: body.error || `error ${res.status}` };
          return;
        }
        const rest = { ...this.fieldErrors };
        delete rest[key];
        this.fieldErrors = rest;
        this.pushToast("saved");
        await this.loadFallback();
      } catch (e) {
        this.fieldErrors = { ...this.fieldErrors, [key]: String(e) };
      }
    },

    async setChain(names) {
      try {
        const res = await fetch("/api/fallback", {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ chain: names }),
        });
        if (!res.ok) {
          const body = await res.json().catch(() => ({}));
          this.pushToast(`chain update failed: ${body.error || res.status}`);
          return;
        }
        await this.loadFallback();
      } catch (e) {
        this.pushToast(`chain update failed: ${e}`);
      }
    },

    removeMember(name) {
      const names = this.fallbackData.chain.map((m) => m.name).filter((n) => n !== name);
      if (this.fallbackSelected === name) this.fallbackSelected = names[0] || null;
      this.setChain(names);
    },

    addCandidate(name) {
      if (!name) return;
      const names = [...this.fallbackData.chain.map((m) => m.name), name];
      this.fallbackAdding = false;
      this.setChain(names);
    },

    dragStart(index) {
      this._dragIndex = index;
    },

    dragDrop(index) {
      if (this._dragIndex === null || this._dragIndex === index) return;
      const names = this.fallbackData.chain.map((m) => m.name);
      const [moved] = names.splice(this._dragIndex, 1);
      names.splice(index, 0, moved);
      this._dragIndex = null;
      this.setChain(names);
    },
  }));
});

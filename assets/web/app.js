// clauth dashboard — Alpine.js component. No build step: this file is
// embedded verbatim into the binary and served as-is.

const TABS = [
  { id: "overview", label: "Overview" },
  { id: "usage", label: "Usage" },
  { id: "setup", label: "Setup" },
  { id: "fallback", label: "Fallback" },
  { id: "config", label: "Config" },
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

// Mirrors the TUI's `absolute_reset_line` (src/tui/render/format.rs): the
// same instant spelled out in US Eastern and China Standard Time, so a
// US-based and a China-based reader see identical text regardless of the
// machine's own timezone. `Intl` handles the US DST calendar for us instead
// of reimplementing the transition-date math client-side.
function fmtDualTz(iso) {
  if (!iso) return "";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  const fmt = (timeZone) => {
    const parts = new Intl.DateTimeFormat("en-US", {
      timeZone,
      year: "numeric",
      month: "numeric",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      hour12: false,
    }).formatToParts(date);
    const get = (type) => parts.find((p) => p.type === type)?.value ?? "";
    const hour = get("hour") === "24" ? "00" : get("hour");
    return `${get("month")}/${get("day")}/${get("year")} ${hour}:${get("minute")}:${get("second")}`;
  };
  return `[US] Resets ${fmt("America/New_York")}  [CN] Resets ${fmt("Asia/Shanghai")}`;
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
    pluginData: null,
    pluginBusy: false,
    pluginError: null,
    configData: null,
    configFieldErrors: {},
    setupData: null,
    setupFieldErrors: {},
    endpointDrafts: {},
    newProfile: { name: "", base_url: "", api_key: "" },
    createError: null,
    oauthName: "",
    oauthJob: null,
    alibabaOpenFor: null,
    alibabaForm: { site: "domestic", region: "" },
    alibabaJob: null,
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
      if (id === "plugin") this.loadPlugin();
      if (id === "config") this.loadConfig();
      if (id === "setup") this.loadSetup();
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
    fmtDualTz,
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

    // ---- Plugin ----

    async loadPlugin() {
      try {
        const res = await fetch("/api/plugin/status");
        if (!res.ok) return;
        this.pluginData = await res.json();
      } catch {
        // left as-is; selecting the tab again retries
      }
    },

    async pluginInstall() {
      this.pluginBusy = true;
      this.pluginError = null;
      try {
        const res = await fetch("/api/plugin/install", { method: "POST" });
        const body = await res.json().catch(() => ({}));
        if (!res.ok) {
          this.pluginError = body.error || `error ${res.status}`;
        } else {
          this.pushToast("plugin installed");
          await this.loadPlugin();
        }
      } catch (e) {
        this.pluginError = String(e);
      } finally {
        this.pluginBusy = false;
      }
    },

    async pluginSelfHeal() {
      this.pluginBusy = true;
      this.pluginError = null;
      try {
        const res = await fetch("/api/plugin/self-heal", { method: "POST" });
        const body = await res.json().catch(() => ({}));
        if (!res.ok) {
          this.pluginError = body.error || `error ${res.status}`;
        } else {
          this.pushToast("self-heal complete");
          await this.loadPlugin();
        }
      } catch (e) {
        this.pluginError = String(e);
      } finally {
        this.pluginBusy = false;
      }
    },

    // ---- Config ----

    async loadConfig() {
      try {
        const res = await fetch("/api/config");
        if (!res.ok) return;
        this.configData = await res.json();
      } catch {
        // left as-is; selecting the tab again retries
      }
    },

    async patchConfig(field, value) {
      try {
        const res = await fetch("/api/config", {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ [field]: value }),
        });
        if (!res.ok) {
          const body = await res.json().catch(() => ({}));
          this.configFieldErrors = { ...this.configFieldErrors, [field]: body.error || `error ${res.status}` };
          return;
        }
        const rest = { ...this.configFieldErrors };
        delete rest[field];
        this.configFieldErrors = rest;
        this.pushToast("saved");
        await this.loadConfig();
      } catch (e) {
        this.configFieldErrors = { ...this.configFieldErrors, [field]: String(e) };
      }
    },

    // ---- Setup ----

    async loadSetup() {
      try {
        const res = await fetch("/api/profiles");
        if (!res.ok) return;
        this.setupData = await res.json();
      } catch {
        // left as-is; selecting the tab again retries
      }
    },

    async createProfile() {
      this.createError = null;
      try {
        const res = await fetch("/api/profiles", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            name: this.newProfile.name,
            base_url: this.newProfile.base_url || null,
            api_key: this.newProfile.api_key || null,
          }),
        });
        const body = await res.json().catch(() => ({}));
        if (!res.ok) {
          this.createError = body.error || `error ${res.status}`;
          return;
        }
        this.pushToast(`created ${this.newProfile.name}`);
        this.newProfile = { name: "", base_url: "", api_key: "" };
        await this.loadSetup();
      } catch (e) {
        this.createError = String(e);
      }
    },

    // The backend patches base_url + api_key TOGETHER (see EndpointPatch in
    // src/web/profiles.rs) — there is no "leave the key alone" value, so
    // sending one without the other WOULD null out whichever is omitted.
    // The UI never sees a stored key back (only `has_api_key`), so an edit
    // always collects both fields explicitly rather than firing off a lone
    // base_url change that could silently wipe an existing key.
    startEndpointEdit(p) {
      this.endpointDrafts = { ...this.endpointDrafts, [p.name]: { base_url: p.base_url || "", api_key: "" } };
    },

    cancelEndpointEdit(name) {
      const rest = { ...this.endpointDrafts };
      delete rest[name];
      this.endpointDrafts = rest;
    },

    async saveEndpointEdit(name, hadApiKey) {
      const draft = this.endpointDrafts[name];
      const key = `${name}.endpoint`;
      if (hadApiKey && !draft.api_key) {
        this.setupFieldErrors = {
          ...this.setupFieldErrors,
          [key]: "re-enter the API key to save (it can't be read back, so it must be re-typed alongside any other change)",
        };
        return;
      }
      try {
        const res = await fetch(`/api/profiles/${encodeURIComponent(name)}`, {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ endpoint: { base_url: draft.base_url || null, api_key: draft.api_key || null } }),
        });
        if (!res.ok) {
          const body = await res.json().catch(() => ({}));
          this.setupFieldErrors = { ...this.setupFieldErrors, [key]: body.error || `error ${res.status}` };
          return;
        }
        const rest = { ...this.setupFieldErrors };
        delete rest[key];
        this.setupFieldErrors = rest;
        this.cancelEndpointEdit(name);
        this.pushToast("saved");
        await this.loadSetup();
      } catch (e) {
        this.setupFieldErrors = { ...this.setupFieldErrors, [key]: String(e) };
      }
    },

    async toggleProfileDisabled(name, disabled) {
      try {
        const res = await fetch(`/api/profiles/${encodeURIComponent(name)}`, {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ disabled }),
        });
        if (!res.ok) {
          const body = await res.json().catch(() => ({}));
          this.pushToast(`update failed: ${body.error || res.status}`);
          return;
        }
        await this.loadSetup();
      } catch (e) {
        this.pushToast(`update failed: ${e}`);
      }
    },

    async deleteSetupProfile(name) {
      try {
        const res = await fetch(`/api/profiles/${encodeURIComponent(name)}`, { method: "DELETE" });
        if (!res.ok) {
          const body = await res.json().catch(() => ({}));
          this.pushToast(`delete failed: ${body.error || res.status}`);
          return;
        }
        this.pushToast(`deleted ${name}`);
        await this.loadSetup();
      } catch (e) {
        this.pushToast(`delete failed: ${e}`);
      }
    },

    startJobPolling(jobId, stateKey) {
      const startedAt = Date.now();
      this[stateKey] = { id: jobId, status: "pending", elapsed: 0, error: null };
      const tick = setInterval(() => {
        if (!this[stateKey] || this[stateKey].id !== jobId) {
          clearInterval(tick);
          return;
        }
        this[stateKey] = { ...this[stateKey], elapsed: Math.round((Date.now() - startedAt) / 1000) };
      }, 1000);
      const poll = async () => {
        if (!this[stateKey] || this[stateKey].id !== jobId) return;
        try {
          const res = await fetch(`/api/jobs/${jobId}`);
          const body = await res.json().catch(() => ({}));
          if (body.status === "succeeded") {
            this[stateKey] = { ...this[stateKey], status: "succeeded" };
            clearInterval(tick);
            this.pushToast("login succeeded");
            await this.loadSetup();
            return;
          }
          if (body.status === "failed") {
            this[stateKey] = { ...this[stateKey], status: "failed", error: body.error };
            clearInterval(tick);
            return;
          }
        } catch {
          // transient network hiccup; keep polling
        }
        setTimeout(poll, 1500);
      };
      poll();
    },

    async startOauthLogin() {
      if (!this.oauthName) return;
      try {
        const res = await fetch("/api/login/oauth", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ name: this.oauthName }),
        });
        const body = await res.json().catch(() => ({}));
        if (!res.ok) {
          this.pushToast(`login failed: ${body.error || res.status}`);
          return;
        }
        this.startJobPolling(body.job_id, "oauthJob");
      } catch (e) {
        this.pushToast(`login failed: ${e}`);
      }
    },

    async startAlibabaLogin(name) {
      try {
        const res = await fetch(`/api/profiles/${encodeURIComponent(name)}/login/alibaba`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ site: this.alibabaForm.site, region: this.alibabaForm.region }),
        });
        const body = await res.json().catch(() => ({}));
        if (!res.ok) {
          this.pushToast(`login failed: ${body.error || res.status}`);
          return;
        }
        this.alibabaOpenFor = null;
        this.startJobPolling(body.job_id, "alibabaJob");
      } catch (e) {
        this.pushToast(`login failed: ${e}`);
      }
    },

  }));
});

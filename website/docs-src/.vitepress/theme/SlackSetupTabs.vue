<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";

type Transport = "socket" | "http";

const selected = ref<Transport>("socket");
const socketButton = ref<HTMLButtonElement>();
const httpButton = ref<HTMLButtonElement>();
const socketPanel = ref<HTMLElement>();
const httpPanel = ref<HTMLElement>();

function select(transport: Transport) {
  selected.value = transport;
  (transport === "socket" ? socketButton.value : httpButton.value)?.focus();
}

function followHash() {
  const target = document.getElementById(window.location.hash.slice(1));
  if (target && httpPanel.value?.contains(target)) selected.value = "http";
  else if (target && socketPanel.value?.contains(target)) selected.value = "socket";
}

onMounted(() => {
  followHash();
  window.addEventListener("hashchange", followHash);
});
onUnmounted(() => window.removeEventListener("hashchange", followHash));
</script>

<template>
  <div class="setup-tabs">
    <div
      class="tab-list"
      role="tablist"
      aria-label="Slack setup"
      @keydown.left.prevent="select(selected === 'socket' ? 'http' : 'socket')"
      @keydown.right.prevent="select(selected === 'socket' ? 'http' : 'socket')"
      @keydown.home.prevent="select('socket')"
      @keydown.end.prevent="select('http')"
    >
      <button
        id="slack-socket-tab"
        ref="socketButton"
        type="button"
        role="tab"
        aria-controls="slack-socket-setup"
        :aria-selected="selected === 'socket'"
        :tabindex="selected === 'socket' ? 0 : -1"
        @click="select('socket')"
      >
        Socket Mode
      </button>
      <button
        id="slack-http-tab"
        ref="httpButton"
        type="button"
        role="tab"
        aria-controls="slack-http-setup"
        :aria-selected="selected === 'http'"
        :tabindex="selected === 'http' ? 0 : -1"
        @click="select('http')"
      >
        HTTP
      </button>
    </div>
    <div
      id="slack-socket-setup"
      ref="socketPanel"
      v-show="selected === 'socket'"
      role="tabpanel"
      aria-labelledby="slack-socket-tab"
      tabindex="0"
    >
      <slot name="socket" />
    </div>
    <div
      id="slack-http-setup"
      ref="httpPanel"
      v-show="selected === 'http'"
      role="tabpanel"
      aria-labelledby="slack-http-tab"
      tabindex="0"
    >
      <slot name="http" />
    </div>
  </div>
</template>

<style scoped>
.tab-list {
  display: flex;
  gap: 24px;
  border-bottom: 1px solid var(--vp-c-divider);
}

.tab-list button {
  padding: 12px 0;
  border-bottom: 2px solid transparent;
  color: var(--vp-c-text-2);
  font-weight: 600;
  cursor: pointer;
}

.tab-list button[aria-selected="true"] {
  border-bottom-color: var(--vp-c-brand-1);
  color: var(--vp-c-brand-1);
}

button:focus-visible,
[role="tabpanel"]:focus-visible {
  outline: 2px solid var(--vp-c-brand-1);
  outline-offset: 4px;
}
</style>

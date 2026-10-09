import { defineConfig } from "vite";

export default defineConfig({
  define: {
    __CUSTOM_CONFIG_MARKER__: JSON.stringify("custom-config-took-effect"),
  },
});

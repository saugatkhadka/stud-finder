<template>
  <div>{{ msg }}</div>
</template>

<script lang="ts">
import { helper } from "./util";
export default { name: "Foo" };
</script>

<script setup lang="ts">
import Bar from "./Bar.vue";
import type { Props } from "./types";
const msg = import("./lazy");
</script>

<style scoped>
.a { color: red; }
</style>

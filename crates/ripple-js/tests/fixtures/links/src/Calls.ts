import * as Routes from "$app/utils/routes";
import { router, useForm } from "@inertiajs/react";
import axios from "axios";

export function go(id) {
  window.location = Routes.home_path();
  location.href = Routes.dashboard_path();
  window.location.href = Routes.wallet_path() + "?x=1";
  window.location.assign(Routes.assign_path());
  location.replace(Routes.replace_path());
  window.open(Routes.open_path(), "_blank");
  router.visit(Routes.visit_path());
  router.get(Routes.get_path(), { q: 1 });
  router.visit(Routes.visit_post_path(), { method: "post", data: {} });
  router.post(Routes.post_path(), {});
  router.delete(Routes.destroy_path(id));
  const form = useForm({ a: 1 });
  form.patch(Routes.form_patch_path());
  fetch(Routes.fetch_path());
  axios.get(Routes.axios_path(), { params: {} });
  fetch(withQuery(Routes.wrapped_path()));
  $.ajax({ url: Routes.ajax_path(), type: "POST", success: () => { window.location.href = Routes.after_path(); } });
  history.pushState({}, "", Routes.push_state_path());
  if (location.pathname === Routes.active_path()) return;
}

const items = [
  { label: "Home", href: Routes.items_home_path() },
  { label: "Lib", url: Routes.items_lib_path() },
];

export function fetched() {
  const url = Routes.var_data_path();
  return fetch(url).then((r) => r.json());
}

export function navigated() {
  const next = Routes.var_link_path();
  window.location.href = next;
}

export function built() {
  const libraryUrl = new URL(Routes.library_url());
  libraryUrl.searchParams.set("a", "b");
  window.location.href = libraryUrl.toString();
}

export function unknown() {
  const remembered = useState(Routes.state_path());
  const passed = Routes.passed_path();
  doSomething(passed);
  const unused = Routes.unused_path();
  return Routes.returned_path();
}

export const EXPORTED = Routes.exported_path();
const config = { endpoint: Routes.endpoint_path() };
useQuery({ url: Routes.query_path() });
const pact = { request: { method: "POST", path: "/api/graphql" } };
api().request({ url: Routes.client_request_path() });
switch (location.pathname) {
  case Routes.switch_path():
    break;
}

export function saved(editing: boolean) {
  const postUrl = editing ? Routes.var_post_a_path() : Routes.var_post_b_path();
  form.post(postUrl, {});
}

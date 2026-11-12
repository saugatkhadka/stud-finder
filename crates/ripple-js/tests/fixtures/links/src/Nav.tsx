import * as Routes from "$app/utils/routes";
import { Link, router, useForm, Form } from "@inertiajs/react";
import axios from "axios";

export const Footer = ({ user }) => (
  <div>
    <a role="menuitem" href={Routes.root_url()}>home</a>
    <Link href={Routes.library_path()}>library</Link>
    <NavLink to={Routes.settings_path()} />
    <img src={Routes.avatar_url(user.id)} />
    <a href={`${Routes.profile_url()}?tab=posts`}>profile</a>
    <a href={cond ? Routes.a_path() : Routes.b_path()}>ab</a>
    <Link href={Routes.logout_path()} method="delete" as="button">log out</Link>
    <Form action={Routes.checkout_path()} method="post" />
    <form action={Routes.search_path()} />
    <SearchBox endpoint={Routes.search_api_path()} />
    <p>{Routes.raw_path()}</p>
    <a href="/api/v1/docs">docs</a>
  </div>
);

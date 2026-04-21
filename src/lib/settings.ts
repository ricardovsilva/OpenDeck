export type Settings = {
	version: string;
	language: string;
	brightness: number;
	sleep_timeout_minutes: number;
	rotation: number;
	background: boolean;
	autolaunch: boolean;
	updatecheck: boolean;
	statistics: boolean;
	separatewine: boolean;
	developer: boolean;
	disableelgato: boolean;
	/** Whether the HTTP management API is enabled. */
	mcp_enabled: boolean;
	/** IP address the management API binds to. Defaults to "127.0.0.1". */
	mcp_bind_address: string;
	/** Port the management API listens on. */
	mcp_port: number;
	/**
	 * Optional bearer token. Required when mcp_bind_address is not a loopback address.
	 * null means no token (only allowed for loopback).
	 */
	mcp_token: string | null;
};

import { invoke } from "@tauri-apps/api/core";
import { type Writable, writable } from "svelte/store";

export const settings: Writable<Settings | null> = writable(null);
(async () => settings.set(await invoke("get_settings")))();
export const localisations: Writable<{ [plugin: string]: any } | null> = writable(null);
settings.subscribe(async (value) => {
	if (value) {
		await invoke("set_settings", { settings: value });
		localisations.set(await invoke("get_localisations", { locale: value.language }));
	}
});

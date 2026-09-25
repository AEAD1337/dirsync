import { mount } from 'svelte';
import App from './App.svelte';
import { initToken } from './lib/auth';

// Before anything talks to the server: every request carries the token.
initToken();

mount(App, { target: document.getElementById('app')! });

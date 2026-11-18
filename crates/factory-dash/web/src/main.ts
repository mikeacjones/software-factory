import { mount } from 'svelte';
import '@xyflow/svelte/dist/style.css';
import './app.css';
import App from './App.svelte';
import { start } from './lib/live.svelte';

start();
mount(App, { target: document.getElementById('app')! });

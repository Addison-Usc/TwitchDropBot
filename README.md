## What is this?

This is a command-line tool designed to automatically watch Twitch streams and claim Time-Based Drops for a selected game.

It runs in the background, finds eligible streams, simulates watch time by sending the necessary **GQL** events, and automatically claims drops as they become available.

### How it Works

1. Logs into your Twitch account (saves credentials to `data/save.json`).
2. Fetches active Drop Campaigns and **groups them by game** to ask you to select one.
3. Finds and prioritizes the **best eligible live stream** for that campaign.
4. Simulates "watching" that stream. **Note:** The underlying **GQL** implementation is powered by [**twitch-gql-rs**](https://github.com/this-is-really/twitch-gql-rs).
5. Monitors your drop progress with a **real-time terminal progress bar**.
6. **Automatically claims** the drop once the required time is met, with robust retry logic.
7. Saves claimed drops to `data/cash.json` to avoid re-claiming.

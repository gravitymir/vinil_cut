#!/usr/bin/env python3
"""
Send an mp4 to Telegram as a VIDEO NOTE (round message) from YOUR OWN account.

Why a script: a "video note" (кружочек) is a special message type. You cannot
turn a normal uploaded file into one via the app UI — it must be sent through
the client API with the video_note flag. This uses Telethon (userbot), so the
circle appears as sent by YOU (not a bot).

Requirements for the file (VinilCut's "Video note" mode already produces them):
  * square (e.g. 512x512)
  * <= 60 seconds
  * mp4 (H.264 + AAC)

Setup (once):
  1) pip install telethon
  2) Get API_ID / API_HASH at https://my.telegram.org -> "API development tools"
  3) Fill them below (or set env vars TG_API_ID / TG_API_HASH).
  4) First run asks for your phone number + the login code (creates vinilcut.session).

Usage:
  python send_note.py vinyl.mp4                # -> Saved Messages ("me")
  python send_note.py vinyl.mp4 @some_channel  # -> a chat/channel/username
  # or set a default target via env TG_TARGET
"""

import asyncio
import os
import sys

try:
    from telethon import TelegramClient
except ImportError:
    sys.exit("Telethon is not installed. Run:  pip install telethon")

# ---- fill these (or use env vars) --------------------------------------------
API_ID = int(os.environ.get("TG_API_ID", "0"))          # e.g. 123456
API_HASH = os.environ.get("TG_API_HASH", "")            # e.g. "0123456789abcdef0123456789abcdef"
DEFAULT_TARGET = os.environ.get("TG_TARGET", "me")      # "me" = Saved Messages
# ------------------------------------------------------------------------------


async def main():
    if len(sys.argv) < 2:
        sys.exit("Usage: python send_note.py <video.mp4> [target]")
    video = sys.argv[1]
    target = sys.argv[2] if len(sys.argv) > 2 else DEFAULT_TARGET

    if not os.path.isfile(video):
        sys.exit(f"File not found: {video}")
    if not API_ID or not API_HASH:
        sys.exit("Set API_ID and API_HASH (get them at https://my.telegram.org).")

    async with TelegramClient("vinilcut.session", API_ID, API_HASH) as client:
        await client.send_file(target, video, video_note=True)
        print(f"Sent '{video}' as a video note to {target}.")


if __name__ == "__main__":
    asyncio.run(main())

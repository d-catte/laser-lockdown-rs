# Laser Lockdown
This repository contains the infrastructure for the ONU makerspace's lockbox.

The backend is written in high-performance embedded Rust while the frontend is written
in simplistic HTML, CSS, and JavaScript.

## Features
- Integrated administration panel
- Log keeping
- Secure password hashing
- Interactable keycard database (add, remove, edit)
- Quickly add keycards with the tap of a button
- Indicator LEDs for actions
  - Triple blink: keycard successfully added
  - Continuous rapid blink: door can be opened
  - Long pulse: access denied
- Indicator buzzer for actions
  - Triple alarm: keycard successfully added
  - Continuous rapid alarm: door can be opened
  - Long alarm: access denied
- Up to 32 users can be added 
- Up to 128 lines can be logged at once
- Accurate log timestamps due to integrated ntp client
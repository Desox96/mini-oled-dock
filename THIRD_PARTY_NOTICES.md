# Third-party notices

mini-oled-dock contains a few pieces adapted from the following project.

## lightbar

Source: https://github.com/SauerExe/lightbar

Adapted: the `NOTIFYICONDATA` / `SHELLTRAYDATA` / `APPBARDATA` wire-format structures and the icon-callback
`encode()` helper in `src/tray_host.rs`.

```
MIT License

Copyright (c) 2026 SauerExe

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## EarTrumpet

Source: https://github.com/File-New-Project/EarTrumpet (MIT). Only the method order of the undocumented
`IPolicyConfig` COM interface (`src/audio.rs`) follows its interop definition; the declaration itself is
written independently. Its licence is reproduced here as a courtesy.

## IQuietHoursSettings

The `IQuietHoursSettings` interface definition follows Rafael Rivera's gist
https://gist.github.com/riverar/085d98ffb1343e92225a10817109b2e3.

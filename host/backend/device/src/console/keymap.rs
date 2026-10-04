// SPDX-License-Identifier: Apache-2.0
//
// Linux evdev key code -> QEMU "qnum" key number (and a best-effort X11
// keysym), for the boot console's QEMU Extended Key Events.
//
// GENERATED -- do not edit by hand. From QEMU 11.1.2's keycodemapdb
// (host/qemu/src/qemu-11.1.2.tar.xz: subprojects/keycodemapdb/data/
// keymaps.csv, the same table QEMU's own ui/input-keymap-*.c.inc come from),
// with keymap-gen's qnum rule: the "AT set1 keycode" column, an 0xe0XX
// extended code becoming 0x80 | XX. The keysym is the "X11 keysym" column,
// with A-Z lowered to a-z: QEMU reads a letter's case as the shift/caps lock
// state, and a keysym here says nothing about that. Regenerate with:
//
//     python3 - keymaps.csv > keymap.rs.body <<'PY'
//     import csv, sys
//     rows = list(csv.reader(open(sys.argv[1])))[1:]
//     qnum, sym, name = {}, {}, {}
//     for r in rows:
//         if not r[1]: continue
//         lin = int(r[1], 0); name.setdefault(lin, r[0])
//         if r[4]:
//             at1 = int(r[4], 0)
//             qnum.setdefault(lin, 0x80 | (at1 & 0x7f) if at1 > 0x7f else at1)
//         if r[13]:
//             s = int(r[13], 0)
//             if ord('A') <= s <= ord('Z'): s += 0x20
//             sym.setdefault(lin, s)
//     n = max(qnum) + 1
//     ...one "(qnum, keysym), // code NAME" line per code 0..n
//     PY

pub const KEYMAP_LEN: usize = 240;

/// Indexed by Linux evdev code: (QEMU qnum, X11 keysym); 0 = none.
#[rustfmt::skip]
pub static KEYMAP: [(u8, u32); KEYMAP_LEN] = [
    (0x00, 0x0000), // 0 KEY_RESERVED
    (0x01, 0xff1b), // 1 KEY_ESC
    (0x02, 0x0031), // 2 KEY_1
    (0x03, 0x0032), // 3 KEY_2
    (0x04, 0x0033), // 4 KEY_3
    (0x05, 0x0034), // 5 KEY_4
    (0x06, 0x0035), // 6 KEY_5
    (0x07, 0x0036), // 7 KEY_6
    (0x08, 0x0037), // 8 KEY_7
    (0x09, 0x0038), // 9 KEY_8
    (0x0a, 0x0039), // 10 KEY_9
    (0x0b, 0x0030), // 11 KEY_0
    (0x0c, 0x002d), // 12 KEY_MINUS
    (0x0d, 0x003d), // 13 KEY_EQUAL
    (0x0e, 0xff08), // 14 KEY_BACKSPACE
    (0x0f, 0xff09), // 15 KEY_TAB
    (0x10, 0x0071), // 16 KEY_Q
    (0x11, 0x0077), // 17 KEY_W
    (0x12, 0x0065), // 18 KEY_E
    (0x13, 0x0072), // 19 KEY_R
    (0x14, 0x0074), // 20 KEY_T
    (0x15, 0x0079), // 21 KEY_Y
    (0x16, 0x0075), // 22 KEY_U
    (0x17, 0x0069), // 23 KEY_I
    (0x18, 0x006f), // 24 KEY_O
    (0x19, 0x0070), // 25 KEY_P
    (0x1a, 0x005b), // 26 KEY_LEFTBRACE
    (0x1b, 0x005d), // 27 KEY_RIGHTBRACE
    (0x1c, 0xff0d), // 28 KEY_ENTER
    (0x1d, 0xffe3), // 29 KEY_LEFTCTRL
    (0x1e, 0x0061), // 30 KEY_A
    (0x1f, 0x0073), // 31 KEY_S
    (0x20, 0x0064), // 32 KEY_D
    (0x21, 0x0066), // 33 KEY_F
    (0x22, 0x0067), // 34 KEY_G
    (0x23, 0x0068), // 35 KEY_H
    (0x24, 0x006a), // 36 KEY_J
    (0x25, 0x006b), // 37 KEY_K
    (0x26, 0x006c), // 38 KEY_L
    (0x27, 0x003b), // 39 KEY_SEMICOLON
    (0x28, 0x0027), // 40 KEY_APOSTROPHE
    (0x29, 0x0060), // 41 KEY_GRAVE
    (0x2a, 0xffe1), // 42 KEY_SHIFT
    (0x2b, 0x005c), // 43 KEY_BACKSLASH
    (0x2c, 0x007a), // 44 KEY_Z
    (0x2d, 0x0078), // 45 KEY_X
    (0x2e, 0x0063), // 46 KEY_C
    (0x2f, 0x0076), // 47 KEY_V
    (0x30, 0x0062), // 48 KEY_B
    (0x31, 0x006e), // 49 KEY_N
    (0x32, 0x006d), // 50 KEY_M
    (0x33, 0x002c), // 51 KEY_COMMA
    (0x34, 0x002e), // 52 KEY_DOT
    (0x35, 0x002f), // 53 KEY_SLASH
    (0x36, 0xffe2), // 54 KEY_RIGHTSHIFT
    (0x37, 0x00d7), // 55 KEY_KPASTERISK
    (0x38, 0xffe9), // 56 KEY_LEFTALT
    (0x39, 0x0020), // 57 KEY_SPACE
    (0x3a, 0xffe5), // 58 KEY_CAPSLOCK
    (0x3b, 0xffbe), // 59 KEY_F1
    (0x3c, 0xffbf), // 60 KEY_F2
    (0x3d, 0xffc0), // 61 KEY_F3
    (0x3e, 0xffc1), // 62 KEY_F4
    (0x3f, 0xffc2), // 63 KEY_F5
    (0x40, 0xffc3), // 64 KEY_F6
    (0x41, 0xffc4), // 65 KEY_F7
    (0x42, 0xffc5), // 66 KEY_F8
    (0x43, 0xffc6), // 67 KEY_F9
    (0x44, 0xffc7), // 68 KEY_F10
    (0x45, 0xff7f), // 69 KEY_NUMLOCK
    (0x46, 0xff14), // 70 KEY_SCROLLLOCK
    (0x47, 0xffb7), // 71 KEY_KP7
    (0x48, 0xffb8), // 72 KEY_KP8
    (0x49, 0xffb9), // 73 KEY_KP9
    (0x4a, 0xffad), // 74 KEY_KPMINUS
    (0x4b, 0xffb4), // 75 KEY_KP4
    (0x4c, 0xffb5), // 76 KEY_KP5
    (0x4d, 0xffb6), // 77 KEY_KP6
    (0x4e, 0xffab), // 78 KEY_KPPLUS
    (0x4f, 0xffb1), // 79 KEY_KP1
    (0x50, 0xffb2), // 80 KEY_KP2
    (0x51, 0xffb3), // 81 KEY_KP3
    (0x52, 0xffb0), // 82 KEY_KP0
    (0x53, 0xffae), // 83 KEY_KPDOT
    (0x54, 0x0000), // 84 
    (0x76, 0xff2a), // 85 KEY_ZENKAKUHANKAKU
    (0x56, 0x005c), // 86 KEY_102ND
    (0x57, 0xffc8), // 87 KEY_F11
    (0x58, 0xffc9), // 88 KEY_F12
    (0x73, 0x005f), // 89 KEY_RO
    (0x78, 0xff26), // 90 KEY_KATAKANA
    (0x77, 0xff25), // 91 KEY_HIRAGANA
    (0x79, 0xff23), // 92 KEY_HENKAN
    (0x70, 0xff27), // 93 KEY_KATAKANAHIRAGANA
    (0x7b, 0xff22), // 94 KEY_MUHENKAN
    (0x5c, 0xffac), // 95 KEY_KPJPCOMMA
    (0x9c, 0xff8d), // 96 KEY_KPENTER
    (0x9d, 0xffe4), // 97 KEY_RIGHTCTRL
    (0xb5, 0xffaf), // 98 KEY_KPSLASH
    (0x54, 0xff15), // 99 KEY_SYSRQ
    (0xb8, 0xffea), // 100 KEY_RIGHTALT
    (0x5b, 0x0000), // 101 KEY_LINEFEED
    (0xc7, 0xff50), // 102 KEY_HOME
    (0xc8, 0xff52), // 103 KEY_UP
    (0xc9, 0xff55), // 104 KEY_PAGEUP
    (0xcb, 0xff51), // 105 KEY_LEFT
    (0xcd, 0xff53), // 106 KEY_RIGHT
    (0xcf, 0xff57), // 107 KEY_END
    (0xd0, 0xff54), // 108 KEY_DOWN
    (0xd1, 0xff56), // 109 KEY_PAGEDOWN
    (0xd2, 0xff63), // 110 KEY_INSERT
    (0xd3, 0xffff), // 111 KEY_DELETE
    (0xef, 0x0000), // 112 KEY_MACRO
    (0xa0, 0x0000), // 113 KEY_MUTE
    (0xae, 0x0000), // 114 KEY_VOLUMEDOWN
    (0xb0, 0x0000), // 115 KEY_VOLUMEUP
    (0xde, 0x0000), // 116 KEY_POWER
    (0x59, 0xffbd), // 117 KEY_KPEQUAL
    (0xce, 0x0000), // 118 KEY_KPPLUSMINUS
    (0xc6, 0xff13), // 119 KEY_PAUSE
    (0x8b, 0x0000), // 120 KEY_SCALE
    (0x7e, 0x0000), // 121 KEY_KPCOMMA
    (0x72, 0x0000), // 122 KEY_HANGEUL
    (0x71, 0x0000), // 123 KEY_HANJA
    (0x7d, 0x0000), // 124 KEY_YEN
    (0xdb, 0xffe7), // 125 KEY_LEFTMETA
    (0xdc, 0xffe8), // 126 KEY_RIGHTMETA
    (0xdd, 0x0000), // 127 KEY_COMPOSE
    (0xe8, 0x0000), // 128 KEY_STOP
    (0x85, 0x0000), // 129 KEY_AGAIN
    (0x86, 0x0000), // 130 KEY_PROPS
    (0x87, 0x0000), // 131 KEY_UNDO
    (0x8c, 0x0000), // 132 KEY_FRONT
    (0xf8, 0x0000), // 133 KEY_COPY
    (0x64, 0x0000), // 134 KEY_OPEN
    (0x65, 0x0000), // 135 KEY_PASTE
    (0xc1, 0x0000), // 136 KEY_FIND
    (0xbc, 0x0000), // 137 KEY_CUT
    (0xf5, 0xff6a), // 138 KEY_HELP
    (0x9e, 0x0000), // 139 KEY_MENU
    (0xa1, 0x0000), // 140 KEY_CALC
    (0x66, 0x0000), // 141 KEY_SETUP
    (0xdf, 0x0000), // 142 KEY_SLEEP
    (0xe3, 0x0000), // 143 KEY_WAKEUP
    (0x67, 0x0000), // 144 KEY_FILE
    (0x68, 0x0000), // 145 KEY_SENDFILE
    (0x69, 0x0000), // 146 KEY_DELETEFILE
    (0x93, 0x0000), // 147 KEY_XFER
    (0x9f, 0x0000), // 148 KEY_PROG1
    (0x97, 0x0000), // 149 KEY_PROG2
    (0x82, 0x0000), // 150 KEY_WWW
    (0x6a, 0x0000), // 151 KEY_MSDOS
    (0x92, 0x0000), // 152 KEY_SCREENLOCK
    (0x6b, 0x0000), // 153 KEY_DIRECTION
    (0xa6, 0x0000), // 154 KEY_CYCLEWINDOWS
    (0xec, 0x0000), // 155 KEY_MAIL
    (0xe6, 0x0000), // 156 KEY_BOOKMARKS
    (0xeb, 0x0000), // 157 KEY_COMPUTER
    (0xea, 0x0000), // 158 KEY_BACK
    (0xe9, 0x0000), // 159 KEY_FORWARD
    (0xa3, 0x0000), // 160 KEY_CLOSECD
    (0x6c, 0x0000), // 161 KEY_EJECTCD
    (0xfd, 0x0000), // 162 KEY_EJECTCLOSECD
    (0x99, 0x0000), // 163 KEY_NEXTSONG
    (0xa2, 0x0000), // 164 KEY_PLAYPAUSE
    (0x90, 0x0000), // 165 KEY_PREVIOUSSONG
    (0xa4, 0x0000), // 166 KEY_STOPCD
    (0xb1, 0x0000), // 167 KEY_RECORD
    (0x98, 0x0000), // 168 KEY_REWIND
    (0x63, 0x0000), // 169 KEY_PHONE
    (0x00, 0x0000), // 170 KEY_ISO
    (0x81, 0x0000), // 171 KEY_CONFIG
    (0xb2, 0x0000), // 172 KEY_HOMEPAGE
    (0xe7, 0x0000), // 173 KEY_REFRESH
    (0x00, 0x0000), // 174 KEY_EXIT
    (0x00, 0x0000), // 175 KEY_MOVE
    (0x88, 0x0000), // 176 KEY_EDIT
    (0x75, 0x0000), // 177 KEY_SCROLLUP
    (0x8f, 0x0000), // 178 KEY_SCROLLDOWN
    (0xf6, 0x0000), // 179 KEY_KPLEFTPAREN
    (0xfb, 0x0000), // 180 KEY_KPRIGHTPAREN
    (0x89, 0x0000), // 181 KEY_NEW
    (0x8a, 0x0000), // 182 KEY_REDO
    (0x5d, 0x0000), // 183 KEY_F13
    (0x5e, 0x0000), // 184 KEY_F14
    (0x5f, 0x0000), // 185 KEY_F15
    (0x55, 0x0000), // 186 KEY_F16
    (0x83, 0x0000), // 187 KEY_F17
    (0xf7, 0x0000), // 188 KEY_F18
    (0x84, 0x0000), // 189 KEY_F19
    (0x5a, 0x0000), // 190 KEY_F20
    (0x74, 0x0000), // 191 KEY_F21
    (0xf9, 0x0000), // 192 KEY_F22
    (0x6d, 0x0000), // 193 KEY_F23
    (0x6f, 0x0000), // 194 KEY_F24
    (0x95, 0x0000), // 195 
    (0x96, 0x0000), // 196 
    (0x9a, 0x0000), // 197 
    (0x9b, 0x0000), // 198 
    (0xa7, 0x0000), // 199 
    (0xa8, 0x0000), // 200 KEY_PLAYCD
    (0xa9, 0x0000), // 201 KEY_PAUSECD
    (0xab, 0x0000), // 202 KEY_PROG3
    (0xac, 0x0000), // 203 KEY_PROG4
    (0xad, 0x0000), // 204 KEY_DASHBOARD
    (0xa5, 0x0000), // 205 KEY_SUSPEND
    (0xaf, 0x0000), // 206 KEY_CLOSE
    (0xb3, 0x0000), // 207 KEY_PLAY
    (0xb4, 0x0000), // 208 KEY_FASTFORWARD
    (0xb6, 0x0000), // 209 KEY_BASSBOOST
    (0xb9, 0x0000), // 210 KEY_PRINT
    (0xba, 0x0000), // 211 KEY_HP
    (0xbb, 0x0000), // 212 KEY_CAMERA
    (0xbd, 0x0000), // 213 KEY_SOUND
    (0xbe, 0x0000), // 214 KEY_QUESTION
    (0xbf, 0x0000), // 215 KEY_EMAIL
    (0xc0, 0x0000), // 216 KEY_CHAT
    (0xe5, 0x0000), // 217 KEY_SEARCH
    (0xc2, 0x0000), // 218 KEY_CONNECT
    (0xc3, 0x0000), // 219 KEY_FINANCE
    (0xc4, 0x0000), // 220 KEY_SPORT
    (0xc5, 0x0000), // 221 KEY_SHOP
    (0x94, 0x0000), // 222 KEY_ALTERASE
    (0xca, 0x0000), // 223 KEY_CANCEL
    (0xcc, 0x0000), // 224 KEY_BRIGHTNESSDOWN
    (0xd4, 0x0000), // 225 KEY_BRIGHTNESSUP
    (0xed, 0x0000), // 226 KEY_MEDIA
    (0xd6, 0x0000), // 227 KEY_SWITCHVIDEOMODE
    (0xd7, 0x0000), // 228 KEY_KBDILLUMTOGGLE
    (0xd8, 0x0000), // 229 KEY_KBDILLUMDOWN
    (0xd9, 0x0000), // 230 KEY_KBDILLUMUP
    (0xda, 0x0000), // 231 KEY_SEND
    (0xe4, 0x0000), // 232 KEY_REPLY
    (0x8e, 0x0000), // 233 KEY_FORWARDMAIL
    (0xd5, 0x0000), // 234 KEY_SAVE
    (0xf0, 0x0000), // 235 KEY_DOCUMENTS
    (0xf1, 0x0000), // 236 KEY_BATTERY
    (0xf2, 0x0000), // 237 KEY_BLUETOOTH
    (0xf3, 0x0000), // 238 KEY_WLAN
    (0xf4, 0x0000), // 239 KEY_UWB
];

/// The QEMU qnum for evdev `code`, if it has one.
pub fn qnum(code: u16) -> Option<u8> {
    KEYMAP.get(code as usize).map(|e| e.0).filter(|&q| q != 0)
}

/// A best-effort X11 keysym for evdev `code` (0 = none known).
pub fn keysym(code: u16) -> u32 {
    KEYMAP.get(code as usize).map_or(0, |e| e.1)
}

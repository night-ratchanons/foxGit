# ปล่อยเวอร์ชันใหม่ (สำหรับผู้ดูแลโปรเจกต์)

## ตั้งค่าครั้งแรก

1. **สร้าง repo บน GitHub แบบ public** แล้ว push โค้ดขึ้นไป
   ```bash
   git remote add origin https://github.com/<owner>/foxgit.git
   git push -u origin main
   ```
   ไม่ต้องแก้ชื่อ repo ในโค้ด ตอน build บน GitHub Actions ลิงก์ About / Report a problem / ตัวเช็กอัปเดต
   จะใช้ชื่อ repo ที่ build อยู่ให้เอง (`GITHUB_REPOSITORY`)

2. **(ไม่บังคับ) Sign + notarize บน macOS** — ต้องมี Apple Developer Program ($99/ปี)
   ถ้าไม่ทำ ผู้ใช้ macOS ต้องคลิกขวา → Open ตอนเปิดครั้งแรก
   - สร้างใบรับรอง **Developer ID Application** แล้ว export เป็น `.p12`
   - ใส่ Secrets: `MACOS_CERTIFICATE` (= `base64 -i cert.p12 | pbcopy`), `MACOS_CERTIFICATE_PASSWORD`,
     `MACOS_SIGN_IDENTITY` (เช่น `Developer ID Application: Your Name (TEAMID)`),
     `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD` (app-specific password จาก appleid.apple.com)

## ทุกครั้งที่ปล่อยเวอร์ชัน

1. แก้ `version` ใน `Cargo.toml` (เช่น `0.2.0`) แล้วรัน `cargo build` ให้ `Cargo.lock` อัปเดต
2. เพิ่มหัวข้อ `## [0.2.0] - YYYY-MM-DD` ใน `CHANGELOG.md` (ข้อความส่วนนี้จะกลายเป็น release notes)
3. ถ้าเพิ่ม/อัปเดต dependency ให้สร้างรายการ license ใหม่:
   `python3 scripts/third-party-licenses.py > THIRD-PARTY-LICENSES.txt`
4. Commit แล้วสร้าง tag ให้ตรงกับเวอร์ชัน:
   ```bash
   git commit -am "Release 0.2.0"
   git tag v0.2.0
   git push origin main v0.2.0
   ```
5. GitHub Actions (workflow **Release**) จะ build ทั้ง 3 ระบบแล้วสร้าง **draft release** พร้อมไฟล์:
   `FoxGit-0.2.0-macos.dmg`, `FoxGit-0.2.0-windows-x64.zip`, `foxgit_0.2.0_amd64.deb`,
   `FoxGit-0.2.0-linux-x64.tar.gz`, `SHA256SUMS.txt`
6. เปิดหน้า Releases บน GitHub ตรวจ draft แล้วกด **Publish release**
   หลังจากนั้นผู้ใช้เวอร์ชันเก่าจะเห็นปุ่ม Update ภายใน 1 วัน

ทดสอบ build บนเครื่องก่อนปล่อยได้ด้วย `scripts/bundle-macos.sh --universal --dmg` (macOS)

## Icon

ต้นฉบับคือ `assets/icon/foxgit.svg` ถ้าแก้ให้สร้างไฟล์ทุกขนาดใหม่ (`.png`, `.icns`, `.ico`):
`cargo install resvg && scripts/make-icons.sh`

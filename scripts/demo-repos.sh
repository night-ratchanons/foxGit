#!/usr/bin/env bash
# Create throw-away demo repositories (for screenshots / manual testing) under $1
# (default /tmp/foxgit-demo/Projects). Never touches your real projects.
set -euo pipefail
ROOT=${1:-/tmp/foxgit-demo/Projects}
rm -rf "$ROOT" && mkdir -p "$ROOT"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
# never fall back to the machine's own name/email (merges, tags …)
export GIT_AUTHOR_NAME=Mali GIT_AUTHOR_EMAIL=mali@example.com
export GIT_COMMITTER_NAME=Mali GIT_COMMITTER_EMAIL=mali@example.com
export GIT_AUTHOR_DATE GIT_COMMITTER_DATE
T=1793000000
c() { # c <author> <message>
  T=$((T + 3600 * 5)); GIT_AUTHOR_DATE="@$T +0700"; GIT_COMMITTER_DATE="$GIT_AUTHOR_DATE"
  GIT_AUTHOR_NAME="$1" GIT_AUTHOR_EMAIL="$(echo "$1" | tr 'A-Z ' 'a-z.')@example.com" \
    git commit -qm "$2"
}
w() { mkdir -p "$(dirname "$1")"; printf '%s\n' "$2" >> "$1"; git add "$1"; }

# shop-api — branch folders, merges, tags, a remote
git init -q -b main "$ROOT/shop-api" && cd "$ROOT/shop-api"
w README.md "# Shop API"; c "Mali" "Initial project layout"
w src/main.rs 'fn main() { println!("shop"); }'; c "Mali" "Add HTTP server skeleton"
w src/db.rs "pub fn connect() {}"; c "Krit" "Connect to Postgres"
git tag v0.1.0
git checkout -qb feature/login
w src/auth.rs "pub fn login() {}"; c "Nok" "Add login endpoint"
w src/auth.rs "pub fn logout() {}"; c "Nok" "Add logout and session expiry"
git checkout -q main
w src/orders.rs "pub fn list() {}"; c "Mali" "List orders with pagination"
git checkout -qb fix/checkout-crash
w src/orders.rs "// guard against empty carts"; c "Lek" "Fix crash when the cart is empty"
git checkout -q main && git merge -q --no-ff fix/checkout-crash -m "Merge branch 'fix/checkout-crash'"
git merge -q --no-ff feature/login -m "Merge branch 'feature/login'"
git tag v0.2.0
git checkout -qb feature/payment
w src/payment.rs "pub fn charge() {}"; c "Krit" "Card payments with 3-D Secure"
w src/payment.rs "pub fn refund() {}"; c "Krit" "Refunds and partial refunds"
git checkout -qb feature/payment-webhooks
w src/webhooks.rs "pub fn handle() {}"; c "Somchai" "Verify payment webhooks"
git checkout -q main
w src/search.rs "pub fn search() {}"; c "Mali" "Product search with filters"
git checkout -qb develop
w CHANGELOG.md "## Unreleased"; c "Nok" "Start changelog"
git checkout -q main
w src/main.rs "// health check"; c "Lek" "Health check endpoint"
git init -q --bare "$ROOT/.remotes/shop-api.git"
git remote add origin "$ROOT/.remotes/shop-api.git"
git push -q origin main develop feature/login feature/payment 2>/dev/null
git fetch -q origin
w src/main.rs "// rate limiting"; c "Mali" "Rate limit public endpoints"

# mobile-app — uncommitted changes (File Status)
git init -q -b main "$ROOT/mobile-app" && cd "$ROOT/mobile-app"
w README.md "# Mobile app"; c "Ploy" "Initial commit"
w src/App.tsx "export default function App() { return null }"; c "Ploy" "App shell"
w src/screens/Home.tsx "export const Home = () => null"; c "Ploy" "Home screen"
git checkout -qb feature/onboarding
w src/screens/Welcome.tsx "export const Welcome = () => null"; c "Ploy" "Welcome screen"
printf 'export const Home = () => {\n  const items = useItems()\n  return <List items={items} />\n}\n' > src/screens/Home.tsx
printf 'export function useItems() {\n  return []\n}\n' > src/hooks.ts && git add src/hooks.ts
printf '# Mobile app\n\nRun `npm start`.\n' > README.md

# landing-page — small and clean
git init -q -b main "$ROOT/landing-page" && cd "$ROOT/landing-page"
w index.html "<h1>Hello</h1>"; c "Mint" "Landing page"
w style.css "body { margin: 0 }"; c "Mint" "Styles"
# web-app — package.json scripts (Run panel) and a merge stopped on a conflict
git init -q -b main "$ROOT/web-app" && cd "$ROOT/web-app"
printf '{\n  "name": "web-app",\n  "scripts": {\n    "dev": "node server.js",\n    "build": "echo building",\n    "test": "echo ok",\n    "lint": "echo lint"\n  }\n}\n' > package.json
touch pnpm-lock.yaml && mkdir -p node_modules && echo node_modules > .gitignore
printf "require('http').createServer((q, r) => r.end('hi')).listen(5173, () => console.log('  Local:   http://localhost:5173/'))\n" > server.js
printf 'export const price = (p) => {\n  return p.toFixed(2)\n}\n\nexport const total = (items) => items.length\n' > src.js
git add -A && c "Mali" "Web app with a dev server"
git checkout -qb feature/currency
printf 'export const price = (p, cur = "THB") => {\n  return `${p.toFixed(2)} ${cur}`\n}\n\nexport const total = (items) => items.length\n' > src.js
git commit -qam "Show the currency" --author "Nok <nok@example.com>"
git checkout -q main
printf 'export const price = (p) => {\n  return "฿" + p.toLocaleString()\n}\n\nexport const total = (items) => items.length\n' > src.js
git commit -qam "Format prices with separators" --author "Mali <mali@example.com>"
git merge -q feature/currency >/dev/null 2>&1 || true

echo "Demo repositories created in $ROOT"

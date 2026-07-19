# MineRider Control — panel Lua

Lokalny panel WWW sterujący swarmem uruchomionym przez binarkę
**minerider-lua**. Ten moduł zastępuje zależność od Mineflayera: logika
połączenia, stanu gracza, czatu, ekwipunku i GUI korzysta wyłącznie z
publicznego wrappera Lua MineRider.

## Zakres

- wiele kont w jednym runtime; ta paczka używa jednego workera Lua,
- logowanie AuthMe, tryb rejestracji i akceptacja regulaminu,
- wybór BOXPVP przez serwerowe GUI,
- reconnect zgodny z polityką MineRider,
- statusy i logi na żywo przez Socket.IO,
- trwałe logi panelu, procesu i osobne logi botów w katalogu `logs`,
- akceptacja resource packów jak w vanilla, domyślnie włączona i możliwa do wyłączenia,
- nowy responsywny panel operatora,
- hasło wyłącznie ze zmiennej środowiskowej,
- import kont ze starego pliku konfiguracyjnego.

Automatyczne obchodzenie weryfikacji/anti-AFK oraz operacje farmienia
ekonomii nie są wykonywane. Gdy serwer zażąda takiej weryfikacji, panel
ustawia status **verification_required**. Używaj wyłącznie na serwerze, na
którym masz zgodę operatora na automatyzację.

## Instalacja

Umieść katalog jako:

~~~text
Minerider/
  apps/
    anarchia-panel/
~~~

Zbuduj MineRider z wrapperem Lua:

~~~bash
cargo build --release --features lua
cd apps/anarchia-panel
npm install
~~~

Skopiuj konfigurację przykładową:

~~~bash
cp config.example.json config.local.json
~~~

Na Windows PowerShell:

~~~powershell
Copy-Item config.example.json config.local.json
$env:MINERIDER_BOT_PASSWORD = "twoje-haslo"
npm start
~~~

Na Linux/macOS:

~~~bash
export MINERIDER_BOT_PASSWORD='twoje-haslo'
npm start
~~~

Panel domyślnie działa tylko lokalnie pod adresem
http://127.0.0.1:3000.

Przycisk **Pobierz logi** zapisuje plik `minerider-diagnostics.log`, który
można przesłać do diagnozy. Hasło botów jest automatycznie redagowane.

## Migracja starej listy kont

~~~bash
npm run migrate -- "/ścieżka/do/starego/config.json"
~~~

Migrator przenosi adres serwera i nicki, ale celowo nie kopiuje hasła,
proxy, logów, cache ani zależności Mineflayera.

## Kontrola jakości

~~~bash
npm run check
npm test
~~~

Polecenie **check** sprawdza składnię wszystkich plików JavaScript i
generowanego skryptu Lua. Testy obejmują walidację konfiguracji, bezpieczne
generowanie Lua, parser telemetrii i redakcję hasła.

## Ograniczenie wrappera

Obecny wrapper nie udostępnia HTTP/WebSocket ani ponownego połączenia dla
zatrzymanego pojedynczego bota. Z tego powodu wybór kont odbywa się przed
startem, a przycisk Stop zatrzymuje cały swarm. Jest to zgodne z publicznym
API opisanym w dokumentacji MineRider.

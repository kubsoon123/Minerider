# Minimal RCON client (Source RCON protocol) for driving the local test
# server: sends one command, prints the response. Localhost only.
# Usage: rcon.ps1 -Command "tp MineRiderTest 10 100 10"
param(
    [Parameter(Mandatory = $true)][string]$Command,
    [string]$Password = "minerider-local-test",
    [string]$HostName = "127.0.0.1",
    [int]$Port = 25575
)
$ErrorActionPreference = "Stop"

function Send-RconPacket($stream, [int]$id, [int]$type, [string]$payload) {
    $payloadBytes = [System.Text.Encoding]::ASCII.GetBytes($payload)
    $ms = New-Object System.IO.MemoryStream
    $bw = New-Object System.IO.BinaryWriter $ms
    $bw.Write([int]$id)
    $bw.Write([int]$type)
    $bw.Write($payloadBytes)
    $bw.Write([byte]0)
    $bw.Write([byte]0)
    $body = $ms.ToArray()
    $lenBytes = [System.BitConverter]::GetBytes([int]$body.Length)
    $stream.Write($lenBytes, 0, 4)
    $stream.Write($body, 0, $body.Length)
    $stream.Flush()
}

function Read-RconPacket($stream) {
    $lenBuf = New-Object byte[] 4
    $read = $stream.Read($lenBuf, 0, 4)
    if ($read -ne 4) { throw "RCON connection closed" }
    $len = [System.BitConverter]::ToInt32($lenBuf, 0)
    if ($len -lt 10 -or $len -gt 4110) { throw "RCON invalid packet length $len" }
    $body = New-Object byte[] $len
    $total = 0
    while ($total -lt $len) {
        $n = $stream.Read($body, $total, $len - $total)
        if ($n -le 0) { throw "RCON connection closed mid-packet" }
        $total += $n
    }
    return @{
        Id      = [System.BitConverter]::ToInt32($body, 0)
        Type    = [System.BitConverter]::ToInt32($body, 4)
        Payload = [System.Text.Encoding]::ASCII.GetString($body, 8, $len - 10)
    }
}

$client = New-Object System.Net.Sockets.TcpClient
$client.Connect($HostName, $Port)
$stream = $client.GetStream()
$stream.ReadTimeout = 10000
try {
    # Login (type 3).
    Send-RconPacket $stream 1 3 $Password
    $auth = Read-RconPacket $stream
    if ($auth.Id -eq -1) { throw "RCON authentication failed (wrong password?)" }
    # Command (type 2).
    Send-RconPacket $stream 2 2 $Command
    $response = Read-RconPacket $stream
    Write-Output $response.Payload
} finally {
    $client.Close()
}

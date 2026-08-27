Add-Type -AssemblyName System.Drawing
$ErrorActionPreference = 'Stop'

function New-RoundRect($x, $y, $w, $h, $r) {
  $p = New-Object Drawing.Drawing2D.GraphicsPath
  $d = $r * 2
  $p.AddArc($x, $y, $d, $d, 180, 90)
  $p.AddArc(($x + $w - $d), $y, $d, $d, 270, 90)
  $p.AddArc(($x + $w - $d), ($y + $h - $d), $d, $d, 0, 90)
  $p.AddArc($x, ($y + $h - $d), $d, $d, 90, 90)
  $p.CloseFigure()
  return $p
}

function P($x, $y) { return [Drawing.PointF]::new([float]$x, [float]$y) }

# A memory card, with the verdict as a badge on its corner.
#
# The card is the subject and stays whole; the check sits beside it rather than
# across it. An earlier cut drew the check over the card with a heavy dark halo
# and the halo ate the silhouette -- at any size it read as a tick with grey
# debris behind it. A corner badge is also how every OS already draws "this
# thing has a status", so it survives being shrunk to 16px.
function New-Icon([int]$S) {
  $bmp = New-Object Drawing.Bitmap($S, $S, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [Drawing.Graphics]::FromImage($bmp)
  $g.SmoothingMode = 'AntiAlias'
  $g.Clear([Drawing.Color]::Transparent)

  $ink  = [Drawing.Color]::FromArgb(255, 16, 24, 34)
  $pale = [Drawing.Color]::FromArgb(255, 208, 218, 231)
  $good = [Drawing.Color]::FromArgb(255, 74, 183, 124)

  $ground = New-RoundRect 0 0 ($S - 1) ($S - 1) ($S * 0.22)
  $g.FillPath((New-Object Drawing.SolidBrush($ink)), $ground)
  # Everything after this is held inside the silhouette, so nothing can poke a
  # square corner out past the rounded ground.
  $g.SetClip($ground)

  # --- the card: notch on the top-right, the corner a camera card actually cuts
  $l = $S * 0.20; $r2 = $S * 0.72; $t = $S * 0.15; $b = $S * 0.80; $n = $S * 0.17
  $card = New-Object Drawing.Drawing2D.GraphicsPath
  $card.AddPolygon(@(
    (P $l $t),
    (P ($r2 - $n) $t),
    (P $r2 ($t + $n)),
    (P $r2 $b),
    (P $l $b)
  ))
  $g.FillPath((New-Object Drawing.SolidBrush($pale)), $card)

  # Contacts along the top edge, where the card's own are, and only once there
  # are enough pixels for them to be strokes rather than mud.
  if ($S -ge 64) {
    $fb = New-Object Drawing.SolidBrush($ink)
    for ($i = 0; $i -lt 4; $i++) {
      $fx = $l + $S * 0.045 + $i * $S * 0.105
      $g.FillRectangle($fb, [float]$fx, [float]($t + $S * 0.05), [float]($S * 0.05), [float]($S * 0.13))
    }
  }

  # --- the verdict, as a badge clear of the card
  $cx = $S * 0.680; $cy = $S * 0.695; $rad = $S * 0.225
  # A ring of the ground colour holds the badge off the card so both keep their
  # shapes when the whole thing is 16 pixels wide.
  $ringR = $rad + $S * 0.055
  $g.FillEllipse((New-Object Drawing.SolidBrush($ink)),
    [float]($cx - $ringR), [float]($cy - $ringR), [float]($ringR * 2), [float]($ringR * 2))
  $g.FillEllipse((New-Object Drawing.SolidBrush($good)),
    [float]($cx - $rad), [float]($cy - $rad), [float]($rad * 2), [float]($rad * 2))

  $pts = @(
    (P ($cx - $rad * 0.46) $cy),
    (P ($cx - $rad * 0.10) ($cy + $rad * 0.38)),
    (P ($cx + $rad * 0.50) ($cy - $rad * 0.42))
  )
  $pen = New-Object Drawing.Pen($ink, [float]($S * 0.075))
  $pen.StartCap = 'Round'; $pen.EndCap = 'Round'; $pen.LineJoin = 'Round'
  $g.DrawLines($pen, $pts)

  $g.Dispose()
  return $bmp
}

# Beside this script, so it runs from any clone rather than only the machine it
# was written on.
$out = $PSScriptRoot
$sizes = @(16, 24, 32, 48, 64, 128, 256)
$pngs = @{}
foreach ($s in $sizes) {
  $bmp = New-Icon $s
  $ms = New-Object IO.MemoryStream
  $bmp.Save($ms, [Drawing.Imaging.ImageFormat]::Png)
  $pngs[$s] = $ms.ToArray()
  $ms.Dispose()
  if ($s -eq 16 -or $s -eq 32 -or $s -eq 256) {
    $bmp.Save("$out\preview-$s.png", [Drawing.Imaging.ImageFormat]::Png)
  }
  if ($s -eq 64) {
    $rect = New-Object Drawing.Rectangle(0, 0, 64, 64)
    $data = $bmp.LockBits($rect, [Drawing.Imaging.ImageLockMode]::ReadOnly, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $bytes = New-Object byte[] (64 * 64 * 4)
    [Runtime.InteropServices.Marshal]::Copy($data.Scan0, $bytes, 0, $bytes.Length)
    $bmp.UnlockBits($data)
    for ($i = 0; $i -lt $bytes.Length; $i += 4) {
      $swap = $bytes[$i]; $bytes[$i] = $bytes[$i + 2]; $bytes[$i + 2] = $swap
    }
    [IO.File]::WriteAllBytes("$out\icon-64.rgba", $bytes)
  }
  $bmp.Dispose()
}

$ms = New-Object IO.MemoryStream
$w = New-Object IO.BinaryWriter($ms)
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
foreach ($s in $sizes) {
  $dim = if ($s -eq 256) { 0 } else { $s }
  $w.Write([byte]$dim); $w.Write([byte]$dim)
  $w.Write([byte]0); $w.Write([byte]0)
  $w.Write([uint16]1); $w.Write([uint16]32)
  $w.Write([uint32]$pngs[$s].Length)
  $w.Write([uint32]$offset)
  $offset += $pngs[$s].Length
}
foreach ($s in $sizes) { $w.Write($pngs[$s]) }
$w.Flush()
[IO.File]::WriteAllBytes("$out\sluice.ico", $ms.ToArray())
$w.Dispose(); $ms.Dispose()

"sluice.ico    $((Get-Item "$out\sluice.ico").Length) bytes, $($sizes.Count) sizes"
"icon-64.rgba  $((Get-Item "$out\icon-64.rgba").Length) bytes"

# The Ostrom mark

<img src="favicon.svg" width="96" height="96" alt="Ostrom">

Four bars, one on each edge, face a solid centre square on a 32×32 grid.

| Element | Position | Size |
|---|---|---|
| Top bar | (11, 2) | 10 × 4 |
| Right bar | (26, 11) | 4 × 10 |
| Bottom bar | (11, 26) | 10 × 4 |
| Left bar | (2, 11) | 4 × 10 |
| Centre square | (11, 11) | 10 × 10 |

Bars are 4 units thick and as wide as the centre. Each bar sits 5 units from the centre.

## Why this form

The outer form follows the thesis, and the centre square stays fixed. Ostrom's thesis is that
the users of a commons can govern it themselves.

- **The centre** is the shared resource. Here that is the portfolio being steered.
- **The four bars** are the users. Each one faces the resource from the same distance, and none
  is larger than another.
- **The open corners** show that no frame joins the users and no outside owner holds the
  boundary. They keep their own rules.

## Usage

- `ostrom-mark.svg` fills with `currentColor`, so it takes its colour from the surrounding text.
  Use it inline.
- `favicon.svg` is fixed near-black and switches to near-white under `prefers-color-scheme: dark`.
  Use it where `currentColor` is not inherited: favicons and `<img>` tags.
- The mark is monochrome. Do not add gradients, shadows or rotation, and do not colour the centre
  separately from the bars.
- The mark stays legible down to 16px.

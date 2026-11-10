//! Zed's logo, for the "Open worktree in Zed" actions: the mark alone, without
//! the app icon's tile, so it sits among the SF Symbols beside it.
//!
//! The outline is `assets/images/zed_logo.svg` from the Zed repository, kept
//! here as its path data and drawn at whatever size is asked for.

use objc2::Message;
use objc2::rc::Retained;
use objc2_app_kit::{NSBezierPath, NSColor, NSImage, NSWindingRule};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// Stands in for an SF Symbol name wherever an action names its icon.
pub(crate) const SYMBOL: &str = "zed";

/// The side of the square `PATH` is drawn in.
const VIEW_BOX: f64 = 96.0;

/// One closed outline, filled even-odd. Absolute `M`, `L`, `H`, `V`, `C` and
/// `Z` commands only, which is all the parser below reads.
const PATH: &str = "M9 6C7.34315 6 6 7.34315 6 9V75H0V9C0 4.02944 4.02944 0 9 0H89.3787C93.3878 0 95.3955 4.84715 92.5607 7.68198L43.0551 57.1875H57V51H63V58.6875C63 61.1728 60.9853 63.1875 58.5 63.1875H37.0551L26.7426 73.5H73.5V36H79.5V73.5C79.5 76.8137 76.8137 79.5 73.5 79.5H20.7426L10.2426 90H87C88.6569 90 90 88.6569 90 87V21H96V87C96 91.9706 91.9706 96 87 96H6.62132C2.61224 96 0.604504 91.1529 3.43934 88.318L52.7574 39H39V45H33V37.5C33 35.0147 35.0147 33 37.5 33H58.7574L69.2574 22.5H22.5V60H16.5V22.5C16.5 19.1863 19.1863 16.5 22.5 16.5H75.2574L85.7574 6H9Z";

/// A path command and its absolute coordinates, in `VIEW_BOX` units.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Command {
    Move(f64, f64),
    Line(f64, f64),
    Curve([f64; 6]),
    Close,
}

/// `PATH` as commands; `H` and `V` become lines. `None` on anything else.
fn parse(data: &str) -> Option<Vec<Command>> {
    // Split into letters and numbers: a letter is a token of its own.
    let mut tokens: Vec<String> = Vec::new();
    for ch in data.chars() {
        if ch.is_ascii_alphabetic() {
            tokens.push(ch.to_string());
        } else if ch == ' ' || ch == ',' {
            tokens.push(String::new());
        } else if let Some(last) = tokens
            .last_mut()
            .filter(|t| !t.starts_with(|c: char| c.is_ascii_alphabetic()))
        {
            last.push(ch);
        } else {
            tokens.push(ch.to_string());
        }
    }
    let number =
        |tokens: &mut dyn Iterator<Item = String>| -> Option<f64> { tokens.next()?.parse().ok() };
    let mut tokens = tokens.into_iter().filter(|t| !t.is_empty());
    let mut commands = Vec::new();
    let (mut x, mut y) = (0.0, 0.0);
    while let Some(letter) = tokens.next() {
        let command = match letter.as_str() {
            "M" => {
                (x, y) = (number(&mut tokens)?, number(&mut tokens)?);
                Command::Move(x, y)
            }
            "L" => {
                (x, y) = (number(&mut tokens)?, number(&mut tokens)?);
                Command::Line(x, y)
            }
            "H" => {
                x = number(&mut tokens)?;
                Command::Line(x, y)
            }
            "V" => {
                y = number(&mut tokens)?;
                Command::Line(x, y)
            }
            "C" => {
                let mut points = [0.0; 6];
                for point in &mut points {
                    *point = number(&mut tokens)?;
                }
                (x, y) = (points[4], points[5]);
                Command::Curve(points)
            }
            "Z" => Command::Close,
            _ => return None,
        };
        commands.push(command);
    }
    Some(commands)
}

/// The logo, `side` points square, filled with `color`. A `template` image
/// takes its colour from where it is drawn, as a button's does.
pub(crate) fn image(side: f64, color: &NSColor, template: bool) -> Option<Retained<NSImage>> {
    path_image(
        PATH,
        VIEW_BOX,
        NSWindingRule::EvenOdd,
        side,
        color,
        template,
    )
}

/// Any outline in this file's path dialect -- absolute `M`, `L`, `H`, `V`,
/// `C`, `Z` -- drawn `side` points square from its `view_box`-unit square,
/// filled by `rule` in `color`. `None` when the data uses anything else.
pub(crate) fn path_image(
    data: &str,
    view_box: f64,
    rule: NSWindingRule,
    side: f64,
    color: &NSColor,
    template: bool,
) -> Option<Retained<NSImage>> {
    let commands = parse(data)?;
    let color = color.retain();
    let draw = block2::RcBlock::new(move |bounds: NSRect| -> objc2::runtime::Bool {
        let scale = bounds.size.width.min(bounds.size.height) / view_box;
        let at = |x: f64, y: f64| NSPoint::new(x * scale, y * scale);
        let path = NSBezierPath::bezierPath();
        for command in &commands {
            match *command {
                Command::Move(x, y) => path.moveToPoint(at(x, y)),
                Command::Line(x, y) => path.lineToPoint(at(x, y)),
                Command::Curve([x1, y1, x2, y2, x, y]) => {
                    path.curveToPoint_controlPoint1_controlPoint2(at(x, y), at(x1, y1), at(x2, y2));
                }
                Command::Close => path.closePath(),
            }
        }
        path.setWindingRule(rule);
        color.setFill();
        path.fill();
        objc2::runtime::Bool::YES
    });
    // Flipped: the path's y runs down, as SVG's does.
    let image = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(side, side), true, &draw);
    image.setTemplate(template);
    Some(image)
}

/// Whether `data` parses and stays inside a `view_box` square, for tests.
#[cfg(test)]
pub(crate) fn parses_inside(data: &str, view_box: f64) -> bool {
    let Some(commands) = parse(data) else {
        return false;
    };
    let inside = |v: f64| (0.0..=view_box).contains(&v);
    commands.iter().all(|command| match command {
        Command::Move(x, y) | Command::Line(x, y) => inside(*x) && inside(*y),
        Command::Curve(points) => points.iter().all(|v| inside(*v)),
        Command::Close => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logo_parses_to_one_closed_outline_inside_its_box() {
        let commands = parse(PATH).expect("the path parses");
        assert!(matches!(commands.first(), Some(Command::Move(9.0, 6.0))));
        assert_eq!(commands.last(), Some(&Command::Close));
        assert_eq!(
            commands
                .iter()
                .filter(|c| matches!(c, Command::Move(..)))
                .count(),
            1
        );
        let inside = |v: f64| (0.0..=VIEW_BOX).contains(&v);
        for command in commands {
            match command {
                Command::Move(x, y) | Command::Line(x, y) => {
                    assert!(inside(x) && inside(y), "{command:?}");
                }
                Command::Curve(points) => assert!(points.iter().all(|v| inside(*v)), "{command:?}"),
                Command::Close => {}
            }
        }
        assert_eq!(parse("M0 0Q1 1 2 2"), None, "unknown commands are refused");
    }
}

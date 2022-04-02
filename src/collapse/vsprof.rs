use std::{cmp::Ordering, io};

use log::warn;

use crate::collapse::{common::Occurrences, Collapse};

static START_LINE: &str = "Level,Function Name,Number of Calls,Elapsed Inclusive Time %,Elapsed Exclusive Time %,Avg Elapsed Inclusive Time,Avg Elapsed Exclusive Time,Module Name,";

/// A stack collapser for the output of the Visual Studio built in profiler.
#[derive(Default)]
pub struct Folder {
    /// Function entries on the stack in this entry thus far.
    stack: Vec<(String, usize)>,
}

impl Collapse for Folder {
    fn collapse<R, W>(&mut self, mut reader: R, writer: W) -> io::Result<()>
    where
        R: std::io::BufRead,
        W: std::io::Write,
    {
        // Skip the header
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line)? == 0 {
            warn!("File ended before start of call graph");
            return Ok(());
        };

        let header = String::from_utf8_lossy(&line).to_string();
        if !line_matches_start_line(&header) {
            return invalid_data_error!(
                "Expected first line to be header line\n    {}\nbut instead got\n    {}",
                START_LINE,
                header
            );
        }

        // Process the data
        let mut occurences = Occurrences::new(1);
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let l = String::from_utf8_lossy(&line);
            let line = l.trim_end();
            if line.is_empty() {
                continue;
            } else {
                self.on_line(line, &mut occurences)?;
            }
        }

        self.write_stack(&mut occurences);

        // Write the results
        occurences.write_and_clear(writer)?;

        // Reset the state
        self.stack.clear();
        Ok(())
    }

    fn is_applicable(&mut self, input: &str) -> Option<bool> {
        let line = input
            .lines()
            .next()
            .expect("there is always at least one line (even if empty)");

        Some(line_matches_start_line(line))
    }
}

impl Folder {
    // Parse lines with values in the order as specified by `START_LINE`, comma delimited.
    // Level,Function Name,Number of Calls,...
    // 6,"System.String.IsNullOrEmpty(string)",4,0.00,0.00,0.00,0.00,"mscorlib.dll",
    fn on_line(&mut self, line: &str, occurences: &mut Occurrences) -> io::Result<()> {
        let (depth, remainder) = get_next_number(line)?;

        // Function names are always wrapped in quotes. By trimming the leading double quote, we
        // know that the next double quote is the double quote closing the function name. Splitting
        // on this double quote, we get the function name and the remainder of the line.
        let split = if let Some(remainder) = remainder.strip_prefix('"') {
            remainder.split_once('"')
        } else {
            return invalid_data_error!("Unable to parse function name from line:\n{}", line);
        };

        if let Some((function_name, remainder)) = split {
            let (number_of_calls, _) = get_next_number(remainder)?;

            let prev_depth = self.stack.len();
            // There are 3 separate cases to handle regarding the depth:
            // 1. prev_depth + 1 == depth -> a new function is called, we only need to
            //    store the function name and the number of times it is called from the
            //    outer function
            // 2. prev_depth == depth -> the previous function call was a leaf node, so we
            //    need to save the current stack and replace the top node with our node
            //    call
            // 3. prev_depth > depth -> the previous function call was a leaf node, so we
            //    need to save the current stack and than we need to pop the top nodes
            //    until the top node is our parent (i.e. the function which called us)
            match prev_depth.cmp(&depth) {
                // Case 1
                Ordering::Less => {
                    assert_eq!(prev_depth + 1, depth);
                    self.stack
                        .push((function_name.to_string(), number_of_calls));
                }
                // Case 2
                Ordering::Equal => {
                    self.write_stack(occurences);
                    self.stack.pop();
                    self.stack
                        .push((function_name.to_string(), number_of_calls));
                }
                // Case 3
                Ordering::Greater => {
                    // The Visual Studio profiler outputs the number of times a function is called.
                    //
                    // Let's say we have a function `A()` which is called 500 times, and which
                    // calls a function `B()` 300 times. If we didn't do anything special here,
                    // this would result in `A()` being assigned 800 samples, giving the impression
                    // that `A()` only calls `B()` less than 50% of the time, while in fact it is
                    // called more than 50% of the time.
                    //
                    // To prevent this from happening, we instead subtract the number of calls from
                    // the previous node (in this case `B()`) from the current node (in this case
                    // `A()`. This leaves `A()` with the correct number of 500 samples.
                    //
                    // If the previous number of calls is equal to the current number of calls, we
                    // don't want to write the current top node, because that would duplicate the
                    // number of samples for the current node.
                    let mut prev_number_of_calls = 0;
                    for _ in 0..(prev_depth - depth + 1) {
                        if prev_number_of_calls != self.stack.last().unwrap().1 {
                            self.write_stack(occurences);
                        }
                        prev_number_of_calls = self.stack.pop().unwrap().1;

                        if self.stack.is_empty() {
                            break;
                        }

                        let last = self.stack.len() - 1;
                        let number_of_calls = &self.stack[last].1;
                        if prev_number_of_calls < *number_of_calls {
                            self.stack[last].1 -= prev_number_of_calls;
                        }
                    }

                    self.stack
                        .push((function_name.to_string(), number_of_calls));
                }
            }
        } else {
            return invalid_data_error!("Unable to parse function name from line:\n{}", line);
        }

        Ok(())
    }

    // Store the current stack in `occurences`
    fn write_stack(&self, occurrences: &mut Occurrences) {
        if let Some(nsamples) = self.stack.last().map(|(_, n)| *n).filter(|n| *n > 0) {
            let functions: Vec<_> = self.stack.iter().map(|(f, _)| &f[..]).collect();
            occurrences.insert(functions.join(";"), nsamples);
        }
    }
}

/// Gets the number from the start of the line. This can either be a number <1000, in which case the
/// line doesn't contain double quotes, or the number can be >1000, in which case the line does
/// contain double quotes. In both cases `line` may start with a leading comma, which will be
/// ignored.
///
/// ### Example inputs
/// - Number <1000: `471,91.25,18.39,401.92,81.02,"Raytracer.exe",`
/// - Number >1000: `"2,893,824",54.37,4.21,0.04,0.00,"Raytracer.exe",`
fn get_next_number(line: &str) -> io::Result<(usize, &str)> {
    // Trim the leading comma, if any
    let line = line.strip_prefix(',').unwrap_or(line);

    let mut remove_leading_comma = false;
    let field = if let Some(line) = line.strip_prefix('"') {
        remove_leading_comma = true;
        line.split_once('"')
    } else {
        line.split_once(',')
    };

    if let Some((num, remainder)) = field {
        // Parse the number
        let thousands = num.split(|c: char| !c.is_ascii_digit());
        let mut n = 0;
        for thousand in thousands {
            if n == 0 {
                if thousand.len() > 3 {
                    return invalid_data_error!(
                        "Expected thousands separators for number bigger than 1000, found '{}'",
                        num
                    );
                }
            } else if thousand.len() != 3 {
                return invalid_data_error!(
                    "Floating point numbers are not valid here, expected an integer, found '{}'",
                    num
                );
            }

            n *= 1000;
            if let Ok(num) = thousand.parse::<usize>() {
                n += num;
            } else {
                return invalid_data_error!(
                    "Unable to parse number '{}', expected an integer",
                    num
                );
            }
        }

        if remove_leading_comma {
            // `remainder` still has a leading comma, because the number is >1000. We need to
            // remove it so we are consistent regardless of whether the number was wrapped in
            // double quotes or not.
            if let Some(remainder) = remainder.strip_prefix(',') {
                return Ok((n, remainder));
            }
        }

        return Ok((n, remainder));
    }

    invalid_data_error!("Invalid number in line:\n{}", line)
}

/// Some files may start with the <U+FEFF> character (zero width no-break space). This
/// causes the call to `starts_with` to return false, which in this case isn't what we want.
/// As this character has no influence on the rest of the file, we can safely ignore it.
fn line_matches_start_line(line: &str) -> bool {
    line.trim()
        .trim_start_matches('\u{feff}')
        .starts_with(START_LINE)
}

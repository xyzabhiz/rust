//@ known-bug: #122736
fn main_ref() {
    let array = [(); {
        let mut x = &0;
        let mut n = 0;
        while n < 5 {
            x = &0;
        }
        0
    }];

    let mut ptrs: Vec<*const [u8]> = vec![&array[0..0], &array[0..1], &array, &array[1..]];
}

fn main() {}

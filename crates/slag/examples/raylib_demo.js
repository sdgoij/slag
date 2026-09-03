// The Slag × raylib demo: a paddle-catcher written entirely in JavaScript,
// drawing through the `rl` host module. Balls fall under gravity and can arc
// off the top of the screen before gravity pulls them back; the paddle
// (arrow keys) catches the ones it is under — each catch feeds the ball
// energy, so it comes back faster. Balls that miss the paddle fall off the
// bottom and disappear. Runs until the window closes (Esc or the window's
// close button).

const WIDTH = 800;
const HEIGHT = 450;
const PADDLE_Y = HEIGHT - 40; // the paddle's top edge (the catch surface)
const PADDLE_SPEED = 840; // arrow-key paddle speed (px/s)
const GRAVITY = 900;
const PADDLE_BOOST = 1.15; // base energy multiplier per catch; the ball's
                           // bounciness scales how much of it applies
const MAX_BALL_SPEED = 1200; // runaway guard; sits above spawn + a couple of boosts

class Ball {
    constructor(x, y, vx, vy, radius, color, bounciness) {
        this.x = x;
        this.y = y;
        this.vx = vx;
        this.vy = vy;
        this.radius = radius;
        this.color = color;
        // Restitution 0.5-1.0: how much horizontal energy a wall bounce keeps.
        this.bounciness = bounciness;
    }

    // Advance one frame. Returns false once the ball has dropped past the
    // bottom of the screen (it missed the paddle), so the caller removes it.
    step(dt, paddle) {
        const bottomBefore = this.y + this.radius;
        this.vy += GRAVITY * dt;
        this.x += this.vx * dt;
        this.y += this.vy * dt;

        // Bounce off the side walls, keeping `bounciness` of the horizontal
        // energy. The top is open: a ball can arc off-screen and gravity
        // pulls it back down.
        if (this.x - this.radius < 0) {
            this.x = this.radius;
            this.vx = -this.vx * this.bounciness;
        }
        if (this.x + this.radius > WIDTH) {
            this.x = WIDTH - this.radius;
            this.vx = -this.vx * this.bounciness;
        }

        // Paddle catch: only a ball that reaches the paddle's top surface
        // while falling, horizontally overlapping it, bounces — and the catch
        // feeds it energy. A ball that misses the paddle keeps falling.
        const left = paddle.x - paddle.width / 2;
        const right = paddle.x + paddle.width / 2;
        const bottom = this.y + this.radius;
        if (
            this.vy > 0 &&
            bottomBefore <= PADDLE_Y &&
            bottom >= PADDLE_Y &&
            this.x >= left - this.radius &&
            this.x <= right + this.radius
        ) {
            this.y = PADDLE_Y - this.radius;
            // The paddle always feeds the ball energy; bouncier balls convert
            // more of the boost. `bounciness` 1 reproduces the plain boost.
            const energy = 1 + (PADDLE_BOOST - 1) * this.bounciness;
            this.vx *= energy;
            this.vy = -Math.abs(this.vy) * energy;
            caught += 1;
            const speed = Math.hypot(this.vx, this.vy);
            if (speed > MAX_BALL_SPEED) {
                this.vx = (this.vx * MAX_BALL_SPEED) / speed;
                this.vy = (this.vy * MAX_BALL_SPEED) / speed;
            }
        }

        // Gone once the ball is fully below the screen.
        return this.y - this.radius <= HEIGHT;
    }
}

const PALETTE = [
    "RED", "ORANGE", "YELLOW", "GREEN", "SKYBLUE", "BLUE", "PURPLE", "MAGENTA",
];

const balls = [];
const paddle = { x: WIDTH / 2, width: 120 };
let spawnEvery = 0;
let caught = 0;
let missed = 0;

function spawnBall(x, y) {
    if (balls.length >= 60) return;
    const angle = Math.random() * Math.PI * 2;
    const speed = 500 + Math.random() * 300; // 500-800 px/s per ball
    const radius = 6 + Math.random() * 14;
    const bounciness = 0.5 + Math.random() * 0.5; // 0.5-1.0, per ball
    const color = rl[PALETTE[(balls.length * 7 + (Math.random() * PALETTE.length) | 0) % PALETTE.length]];
    balls.push(new Ball(x, y, Math.cos(angle) * speed, Math.sin(angle) * speed, radius, color, bounciness));
}

rl.initWindow(WIDTH, HEIGHT, "Slag runs raylib");
rl.setTargetFPS(60);

let frame = 0;
while (!rl.windowShouldClose()) {
    const dt = Math.min(rl.getFrameTime(), 0.05);
    frame += 1;

    // Spawn a fresh ball periodically so the scene evolves on its own.
    spawnEvery -= 1;
    if (spawnEvery <= 0) {
        spawnEvery = 40 + (Math.random() * 60 | 0);
        spawnBall(40 + Math.random() * (WIDTH - 80), 60);
    }

    // The paddle follows the arrow keys; the mouse adds a ring marker.
    if (rl.isKeyDown(rl.KEY_LEFT)) paddle.x -= PADDLE_SPEED * dt;
    if (rl.isKeyDown(rl.KEY_RIGHT)) paddle.x += PADDLE_SPEED * dt;
    paddle.x = Math.max(paddle.width / 2, Math.min(WIDTH - paddle.width / 2, paddle.x));
    const mouseX = rl.getMouseX();
    const mouseY = rl.getMouseY();
    if (rl.isMouseButtonPressed(rl.MOUSE_BUTTON_LEFT)) spawnBall(mouseX, mouseY);
    if (rl.isKeyPressed(rl.KEY_SPACE)) spawnBall(paddle.x, PADDLE_Y - 20);

    // Step every ball, dropping the ones the paddle did not catch.
    for (let i = balls.length - 1; i >= 0; i--) {
        if (!balls[i].step(dt, paddle)) {
            balls.splice(i, 1);
            missed += 1;
        }
    }

    rl.beginDrawing();
    rl.clearBackground(rl.DARKGRAY);

    // Title and instructions.
    rl.drawText("Slag runs raylib — catch the balls!", 12, 12, 24, rl.RAYWHITE);
    const hint = "arrows: paddle   click/space: new ball   esc: quit";
    rl.drawText(hint, 12, 44, 16, rl.LIGHTGRAY);
    rl.drawText("caught " + caught + "   missed " + missed + "   balls " + balls.length, 12, HEIGHT - 64, 16, rl.LIGHTGRAY);

    // The balls (as translucent circles with a solid core).
    for (const ball of balls) {
        rl.drawCircle(ball.x | 0, ball.y | 0, ball.radius, ball.color);
    }

    // Paddle + mouse ring.
    rl.drawRectangle(
        (paddle.x - paddle.width / 2) | 0,
        PADDLE_Y | 0,
        paddle.width | 0,
        12,
        rl.LIGHTGRAY,
    );
    rl.drawCircleLines(mouseX, mouseY, 16, rl.GOLD);

    rl.drawFPS(WIDTH - 90, 12);
    rl.endDrawing();
}

console.log("window closed after " + frame + " frames (" + caught + " caught, " + missed + " missed)");
rl.closeWindow();
